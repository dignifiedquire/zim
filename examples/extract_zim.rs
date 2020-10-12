use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use clap::{App, Arg};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use stopwatch::Stopwatch;
use zim::{Cluster, DirectoryEntry, MimeType, Namespace, Target, Zim};

fn main() {
    let matches = App::new("zimextractor")
        .version("0.1")
        .about("Extract zim files")
        .arg(
            Arg::with_name("out")
                .short("o")
                .long("out")
                .help("Output directory")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("skip-link")
                .long("skip-link")
                .help("Skip genrating hard links")
                .takes_value(false),
        )
        .arg(
            Arg::with_name("flatten-link")
                .long("flatten-link")
                .help("Write files to disk, instead of using hard links")
                .takes_value(false),
        )
        .arg(
            Arg::with_name("INPUT")
                .help("Set the zim file to extract")
                .required(true)
                .index(1),
        )
        .get_matches();

    let skip_link = matches.is_present("skip-link");
    let flatten_link = matches.is_present("flatten-link");
    let out = matches.value_of("out").unwrap_or("out");
    let root_output = Path::new(out);

    let input = matches.value_of("INPUT").unwrap();

    println!("Extracting file: {} to {}\n", input, out);
    println!("Generating symlinks: {}", !skip_link);
    println!("Generating copies for links: {}", flatten_link);

    let sw = Stopwatch::start_new();
    let zim_file = Zim::new(input).expect("failed to parse input");

    if let Some(main_page_idx) = zim_file.header.main_page {
        let page = zim_file
            .get_by_url_index(main_page_idx)
            .expect("failed to get main page");
        println!("Main page is {}", page.url);
    }
    println!("");

    let pb = ProgressBar::new(zim_file.article_count() as u64);
    let style = ProgressStyle::default_bar()
        .template(
            "{msg}\n{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
        )
        .progress_chars("#>-");
    pb.set_style(style);

    ensure_dir(root_output);

    // map between cluster and directory entry
    let mut cluster_map = HashMap::new();

    for i in 0..zim_file.header.cluster_count {
        let cluster = zim_file.get_cluster(i).expect("failed to retrieve cluster");
        cluster_map.insert(i, cluster);
    }

    let entries: Vec<_> = zim_file.iterate_by_urls().collect();
    pb.set_message("Writing entries to disk");
    entries
        .par_iter()
        .filter(|entry| {
            if let Some(Target::Cluster(_, _)) = entry.target.as_ref() {
                return true;
            }
            false
        })
        .for_each(|entry| {
            process_file(&root_output, &cluster_map, entry, &pb);
        });

    if !skip_link {
        pb.set_message("Generating links");
        entries
            .par_iter()
            .filter(|entry| {
                if let Some(Target::Redirect(_)) = entry.target.as_ref() {
                    return true;
                }
                false
            })
            .for_each(|entry| {
                process_link(&zim_file, &root_output, entry, skip_link, flatten_link, &pb);
            });
    }

    pb.finish_with_message(&format!(
        "Extraction done in {}s",
        sw.elapsed_ms() as f64 / 1000.
    ));
}

fn safe_write<T: AsRef<[u8]>>(path: &Path, data: T, count: usize) {
    let display = path.display();
    let contain_path = path.parent().unwrap();

    ensure_dir(contain_path);

    match File::create(&path) {
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
                eprintln!(
                    "skipping: couldn't write to {}: {}",
                    display,
                    why.description()
                );
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
        .unwrap_or_else(|e| ignore_exists_err(e, &format!("create: {}", path.display())));
}

fn process_file<'a>(
    root_output: &Path,
    cluster_map: &'a HashMap<u32, Cluster<'a>>,
    entry: &DirectoryEntry,
    pb: &ProgressBar,
) {
    let dst = make_path(root_output, entry.namespace, &entry.url, &entry.mime_type);
    match entry.target.as_ref() {
        Some(Target::Cluster(cluster_index, blob_idx)) => {
            let cluster = cluster_map.get(cluster_index).expect("missing cluster");

            match cluster.get_blob(*blob_idx) {
                Ok(blob) => {
                    safe_write(&dst, blob, 1);
                }
                Err(err) => {
                    eprintln!("skipping invalid blob: {}: {}", dst.display(), err);
                }
            }
            pb.inc(1);
        }
        Some(_) => unreachable!("filtered out earlier"),
        None => {
            eprintln!("skipping missing target {} {:?}", dst.display(), entry);
        }
    }
}
fn process_link(
    zim_file: &Zim,
    root_output: &Path,
    entry: &DirectoryEntry,
    skip_link: bool,
    flatten_link: bool,
    pb: &ProgressBar,
) {
    let dst = make_path(root_output, entry.namespace, &entry.url, &entry.mime_type);

    if entry.target.is_none() {
        eprintln!("skipping missing target {:?} {:?}", dst, entry);
        return;
    }

    match entry.target.as_ref() {
        Some(Target::Redirect(redir)) => {
            if !skip_link && !dst.exists() {
                pb.inc_length(1);

                let entry = {
                    zim_file
                        .get_by_url_index(*redir)
                        .expect("failed to get_by_url_index")
                };
                let src = make_path(root_output, entry.namespace, &entry.url, &entry.mime_type);
                make_link(src, dst, flatten_link);
                pb.inc(1);
            }
        }
        _ => panic!("must be filtered before"),
    }
}

fn make_link(src: PathBuf, mut dst: PathBuf, flatten_link: bool) {
    if !src.exists() {
        eprintln!("Warning: link source doesn't exist: {}", src.display());
    } else if !dst.exists() {
        let contain_path = dst.parent().unwrap();
        ensure_dir(contain_path);

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
    s.push(namespace as u8 as char);
    let mut path = if url.starts_with("/") {
        // make absolute urls relative to the output folder
        let url = url.replacen("/", "", 1);
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
