extern crate clap;
extern crate pbr;
extern crate stopwatch;
extern crate zim;

use clap::{App, Arg};
use pbr::MultiBar;
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use stopwatch::Stopwatch;
use zim::{Target, Zim};

fn main() {
    let matches = App::new("zim-linkr")
        .version("0.1")
        .about("Link ipfs files via 'ipfs files' api")
        .arg(
            Arg::with_name("ROOT")
                .help("Root of the extracted content in the 'ipfs files' api")
                .required(true)
                .index(1),
        )
        .arg(
            Arg::with_name("INPUT")
                .help("The zim file with link data in")
                .required(true)
                .index(2),
        )
        .get_matches();

    let root = matches.value_of("ROOT").unwrap();
    let root_output = Path::new(root);

    let input = matches.value_of("INPUT").unwrap();
    let mut mb = MultiBar::new();

    mb.println(&format!("Linking files using {} into {}:", input, root));
    mb.println("");

    let sw = Stopwatch::start_new();

    let zim = Zim::new(input).ok().unwrap();

    // map between cluster and directory entry
    let mut cluster_map = HashMap::new();

    let mut p1 = mb.create_bar(zim.header.cluster_count as u64);
    let mut p3 = mb.create_bar(zim.header.cluster_count as u64);

    thread::spawn(move || {
        mb.listen();
    });

    p1.show_message = true;
    p1.message("Building cluster map :");

    for i in zim.iterate_by_urls() {
        if let Some(Target::Cluster(cid, _)) = i.target {
            cluster_map.entry(cid).or_insert(Vec::new()).push(i);
        }
        p1.inc();
    }

    p1.finish_print("Created cluster map");

    p3.show_message = true;
    p3.message("Linking redirects :");

    let mut ops = Vec::new();

    // link all redirects
    for entry in zim.iterate_by_urls() {
        // get redirect entry
        if let Some(Target::Redirect(redir)) = entry.target {
            let redir = zim.get_by_url_index(redir).unwrap();

            let mut s = String::new();
            s.push(redir.namespace as u8 as char);
            let src = root_output.join(&s).join(&redir.url);

            let mut d = String::new();
            d.push(entry.namespace as u8 as char);
            let dst = root_output.join(&s).join(&entry.url);

            if src != dst {
                ops.push(format!(
                    "ipfs files cp {} {}",
                    src.to_str().unwrap(),
                    dst.to_str().unwrap()
                ));
            }
        }
        p3.inc();
    }

    let mut f = File::create(PathBuf::from("link.txt")).unwrap();
    f.write_all(ops.join("\n").as_bytes()).unwrap();
    f.sync_data().unwrap();

    p3.finish_print(&format!("Linking done in {}ms", sw.elapsed_ms()));
}
