use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use stopwatch::Stopwatch;
use zim::{DirectoryEntry, MimeType, Namespace, Target, Zim};

/// Extract zim files into their on disk structure.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Output directory.
    #[arg(long, short)]
    out: Option<String>,
    /// Skip generating hard links
    #[arg(long, default_value_t = false)]
    skip_link: bool,
    /// Write files to disk, instead of using hard links
    #[arg(long, default_value_t = false)]
    flatten_link: bool,
    /// Number of clusters to extract in parallel (default: one per core).
    ///
    /// Peak memory is roughly this many decompressed clusters, so lower it for large archives.
    #[arg(long, short)]
    jobs: Option<usize>,
    #[arg(required = true)]
    input: String,
}

fn main() {
    let args = Args::parse();

    if let Some(jobs) = args.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build_global()
            .expect("failed to configure the thread pool");
    }

    let skip_link = args.skip_link;
    let flatten_link = args.flatten_link;
    let out = args.out.unwrap_or_else(|| "out".to_string());
    let root_output = Path::new(&out);

    let input = &args.input;

    println!("Extracting file: {} to {}\n", input, out);
    println!("Generating symlinks: {}", !skip_link);
    println!("Generating copies for links: {}", flatten_link);

    let sw = Stopwatch::start_new();
    let zim_file = Zim::new(input).expect("failed to parse input");

    if let Some(page) = zim_file.main_page().expect("failed to resolve main page") {
        println!("Main page is {}", page.url);
    }
    println!();

    let pb = ProgressBar::new(zim_file.header.article_count as u64);
    pb.enable_steady_tick(Duration::from_millis(100));
    let style = ProgressStyle::default_bar()
        .template(
            "{msg}\n{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
        )
        .unwrap()
        .progress_chars("#>-");
    pb.set_style(style);

    ensure_dir(root_output);

    // Group content entries by the cluster holding them, so extraction runs one cluster at a
    // time per thread: each is decompressed once, written out, then dropped. Holding every
    // cluster - as a map built up front does - keeps the whole archive decompressed at once.
    //
    // Only entry indices are kept. Re-reading a directory entry is a cheap mapped read, whereas
    // holding every entry's path and title costs more than the cluster data does.
    pb.set_message("Scanning entries");
    let mut by_cluster: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut redirects: Vec<u32> = Vec::new();
    let mut unreadable = 0usize;

    for idx in 0..zim_file.header.article_count {
        match zim_file.get_by_url_index(idx) {
            Ok(entry) => match entry.target {
                Some(Target::Cluster(cluster, _)) => {
                    by_cluster.entry(cluster).or_default().push(idx)
                }
                Some(Target::Redirect(_)) => redirects.push(idx),
                None => {}
            },
            Err(err) => {
                unreadable += 1;
                eprintln!("skipping unreadable entry: {}", err);
            }
        }
        pb.inc(1);
    }

    if unreadable > 0 {
        eprintln!(
            "warning: {} of {} entries could not be read",
            unreadable, zim_file.header.article_count
        );
    }

    // Sorted so clusters are taken up in file order, and so a run is reproducible.
    let mut by_cluster: Vec<(u32, Vec<u32>)> = by_cluster.into_iter().collect();
    by_cluster.sort_unstable_by_key(|(cluster, _)| *cluster);

    pb.set_message("Writing entries to disk");
    pb.set_length(by_cluster.iter().map(|(_, e)| e.len() as u64).sum());
    pb.set_position(0);

    by_cluster.par_iter().for_each(|(cluster_idx, entries)| {
        extract_cluster(&zim_file, root_output, *cluster_idx, entries, &pb);
    });

    if !skip_link {
        pb.set_message("Generating links");
        pb.set_length(redirects.len() as u64);
        pb.set_position(0);

        redirects.par_iter().for_each(|&idx| {
            match zim_file.get_by_url_index(idx) {
                Ok(entry) => process_link(&zim_file, root_output, entry, flatten_link),
                Err(err) => eprintln!("skipping unreadable entry: {}", err),
            }
            pb.inc(1);
        });
    }

    pb.finish_with_message(format!(
        "Extraction done in {}s",
        sw.elapsed_ms() as f64 / 1000.
    ));
}

fn safe_write<T: AsRef<[u8]>>(path: &Path, data: T, count: usize) {
    let display = path.display();
    if let Some(contain_path) = path.parent() {
        ensure_dir(contain_path);
    }

    match File::create(path) {
        Err(why) => {
            if count < 3 {
                safe_write(path, data, count + 1);
            } else {
                eprintln!(
                    "skipping: failed retry: couldn't create {}: {:?}",
                    display, why
                );
            }
        }
        Ok(file) => {
            let mut writer = BufWriter::new(&file);

            if let Err(why) = writer.write_all(data.as_ref()) {
                eprintln!("skipping: couldn't write to {}: {}", display, why);
            }
        }
    }
}

fn ensure_dir(path: &Path) {
    if path.exists() {
        // already done
        return;
    }

    std::fs::create_dir_all(path)
        .unwrap_or_else(|e| ignore_exists_err(e, format!("create: {}", path.display())));
}

/// Writes out every entry whose content lives in one cluster.
///
/// The cluster is loaded once here and dropped on return, so its decompressed data does not
/// outlive the entries that need it. Blobs are borrowed from it rather than copied out.
fn extract_cluster(
    zim_file: &Zim,
    root_output: &Path,
    cluster_idx: u32,
    entries: &[u32],
    pb: &ProgressBar,
) {
    // The guard borrows the cluster, so the cluster has to outlive it - and both are dropped on
    // return, which is what frees the decompressed data.
    let cluster = match zim_file.get_cluster(cluster_idx) {
        Ok(cluster) => cluster,
        Err(err) => {
            eprintln!("skipping cluster {}: {}", cluster_idx, err);
            pb.inc(entries.len() as u64);
            return;
        }
    };

    let guard = match cluster.read() {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("skipping cluster {}: {}", cluster_idx, err);
            pb.inc(entries.len() as u64);
            return;
        }
    };

    for &idx in entries {
        let entry = match zim_file.get_by_url_index(idx) {
            Ok(entry) => entry,
            Err(err) => {
                eprintln!("skipping unreadable entry: {}", err);
                pb.inc(1);
                continue;
            }
        };

        if let Some(Target::Cluster(_, blob_idx)) = entry.target {
            let dst = make_path(root_output, entry.namespace, &entry.url, &entry.mime_type);

            match guard.blob(blob_idx) {
                Ok(blob) => safe_write(&dst, blob, 1),
                Err(err) => eprintln!("skipping invalid blob: {}: {}", dst.display(), err),
            }
        }

        pb.inc(1);
    }
}

fn process_link(zim_file: &Zim, root_output: &Path, entry: DirectoryEntry, flatten_link: bool) {
    let dst = make_path(root_output, entry.namespace, &entry.url, &entry.mime_type);
    if dst.exists() {
        return;
    }

    // A redirect may point at another redirect, so follow the chain to the entry actually
    // holding the content - that is the file which exists on disk to link to.
    let target = match zim_file.resolve(entry) {
        Ok(target) => target,
        Err(err) => {
            eprintln!("skipping link {}: {}", dst.display(), err);
            return;
        }
    };

    let src = make_path(
        root_output,
        target.namespace,
        &target.url,
        &target.mime_type,
    );
    make_link(src, dst, flatten_link);
}

fn make_link(src: PathBuf, mut dst: PathBuf, flatten_link: bool) {
    if !src.exists() {
        eprintln!("Warning: link source doesn't exist: {}", src.display());
    } else if !dst.exists() {
        if let Some(contain_path) = dst.parent() {
            ensure_dir(contain_path);
        }

        if let Some(ext) = src.extension() {
            if dst.extension().is_none() || dst.extension().unwrap() != ext {
                dst.set_extension(ext);
            }
        }

        if flatten_link {
            std::fs::copy(&src, &dst).unwrap_or_else(|e| {
                ignore_exists_err(
                    e,
                    format!("copy link: {} -> {}", src.display(), dst.display()),
                );
                0
            });
        } else {
            std::fs::hard_link(&src, &dst).unwrap_or_else(|e| {
                ignore_exists_err(
                    e,
                    format!("create link: {} -> {}", src.display(), dst.display()),
                );
            });
        }
    }
}

fn ignore_exists_err<T: AsRef<str>>(e: std::io::Error, msg: T) {
    use std::io::ErrorKind::*;

    match e.kind() {
        // do not panic if it already exists, that's fine, we just want to make
        // sure we have it before moving on
        AlreadyExists => {}
        _ => {
            eprintln!("skipping: {}: {}", msg.as_ref(), e);
        }
    }
}

fn make_path(root: &Path, namespace: Namespace, url: &str, mime_type: &MimeType) -> PathBuf {
    let mut s = String::new();
    s.push(namespace.as_byte() as char);
    let mut path = if url.starts_with('/') {
        // make absolute urls relative to the output folder
        let url = url.replacen('/', "", 1);
        root.join(&s).join(url)
    } else {
        root.join(&s).join(url)
    };

    if let MimeType::Type(typ) = mime_type {
        let extension = match typ.as_str() {
            "text/html" => Some("html"),
            "image/jpeg" => Some("jpg"),
            "image/png" => Some("png"),
            "image/gif" => Some("gif"),
            "image/svg+xml" => Some("svg"),
            "application/javascript" => Some("js"),
            "text/css" => Some("css"),
            "text/plain" => Some("txt"),
            _ => None,
        };
        if let Some(extension) = extension {
            if path.extension().is_none()
                || !path
                    .extension()
                    .unwrap()
                    .to_str()
                    .unwrap_or_default()
                    .starts_with(extension)
            {
                path.set_extension(extension);
            }
        }
    }

    path
}
