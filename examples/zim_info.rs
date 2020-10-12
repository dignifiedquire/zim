extern crate clap;
extern crate zim;

use clap::{App, Arg};
use zim::Zim;

fn main() {
    let matches = App::new("zim-info")
        .version("0.1")
        .about("Inspect zim files")
        .arg(
            Arg::with_name("INPUT")
                .help("The zim file to inspect")
                .required(true)
                .index(1),
        )
        .get_matches();

    let input = matches.value_of("INPUT").unwrap();

    println!("Inspecting: {}\n", input);

    let zim_file = Zim::new(input).expect("failed to parse input");

    println!("UUID: {}", &zim_file.header.uuid);
    println!("Article Count: {}", zim_file.article_count());
    println!("Mime List Pos: {}", zim_file.header.mime_list_pos);
    println!("URL Pointer Pos: {}", zim_file.header.url_ptr_pos);
    println!("Title Index Pos: {}", zim_file.header.title_ptr_pos);
    println!("Cluster Count: {}", zim_file.header.cluster_count);
    println!("Cluster Pointer Pos: {}", zim_file.header.cluster_ptr_pos);
    println!("Checksum: {}", hex::encode(&zim_file.checksum));
    println!("Checksum Pos: {}", zim_file.header.checksum_pos);

    let (main_page, main_page_idx) = if let Some(main_page_idx) = zim_file.header.main_page {
        let page = zim_file
            .get_by_url_index(main_page_idx)
            .expect("failed to get main page");

        (page.url, main_page_idx as isize)
    } else {
        ("-".into(), -1)
    };

    println!("Main page: '{}' (index: {})", main_page, main_page_idx);

    let (layout_page, layout_page_idx) = if let Some(layout_page_idx) = zim_file.header.layout_page
    {
        let page = zim_file
            .get_by_url_index(layout_page_idx)
            .expect("failed to get layout page");

        (page.url, layout_page_idx as isize)
    } else {
        ("-".into(), -1)
    };

    println!(
        "Layout page: '{}' (index: {})",
        layout_page, layout_page_idx
    );
}
