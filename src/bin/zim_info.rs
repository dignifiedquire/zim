use std::collections::HashSet;

use clap::Parser;
use num_format::{Locale, ToFormattedString};
use zim::{Result, Zim};

/// Inspect zim files
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The zim file to inspect
    input: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let input = &args.input;

    println!("Inspecting: {}\n", input);

    let zim_file = Zim::new(input)?;

    println!(
        "Version {}.{}",
        zim_file.header.version_major, zim_file.header.version_minor
    );

    println!("UUID: {}", zim_file.header.uuid);
    println!(
        "Article Count: {}",
        zim_file
            .header
            .article_count
            .to_formatted_string(&Locale::en)
    );
    println!(
        "Mime List Pos: {}",
        zim_file
            .header
            .mime_list_pos
            .to_formatted_string(&Locale::en)
    );
    println!(
        "URL Pointer Pos: {}",
        zim_file.header.url_ptr_pos.to_formatted_string(&Locale::en)
    );
    println!(
        "Title Index Pos: {:?}",
        zim_file
            .header
            .title_ptr_pos
            .map(|t| t.to_formatted_string(&Locale::en))
    );
    println!(
        "Cluster Count: {}",
        zim_file
            .header
            .cluster_count
            .to_formatted_string(&Locale::en)
    );
    println!("Cluster Pointer Pos: {}", zim_file.header.cluster_ptr_pos);
    println!("Checksum: {}", hex::encode(zim_file.checksum));
    println!(
        "Checksum Pos: {}",
        zim_file
            .header
            .checksum_pos
            .to_formatted_string(&Locale::en)
    );

    let mut compressions = HashSet::new();
    for cluster_id in 0..zim_file.header.cluster_count {
        let cluster = zim_file.get_cluster(cluster_id)?;
        compressions.insert(cluster.compression());
    }
    println!("Compressions: {:?}", compressions);

    let main_page = match zim_file.main_page()? {
        Some(page) => page.url,
        None => "-".into(),
    };
    println!("Main page: \"{}\"", main_page);

    println!(
        "Title listing: {}",
        match (
            zim_file.entry_list_by_title()?,
            zim_file.article_list_by_title()?,
        ) {
            (Some(entries), Some(articles)) => format!(
                "{} entries (v0), {} articles (v1)",
                entries.len(),
                articles.len()
            ),
            (Some(entries), None) => format!("{} entries (v0)", entries.len()),
            (None, Some(articles)) => format!("{} articles (v1)", articles.len()),
            (None, None) => "-".into(),
        }
    );

    println!(
        "Search indexes: fulltext {}, title {}",
        if zim_file.fulltext_index()?.is_some() {
            "yes"
        } else {
            "no"
        },
        if zim_file.title_index()?.is_some() {
            "yes"
        } else {
            "no"
        }
    );

    println!("\nMetadata:");
    for key in zim_file.metadata_keys()? {
        match zim_file.metadata(&key)? {
            // Metadata is text apart from the illustrations, so only print what renders.
            Some(value) => match String::from_utf8(value) {
                Ok(text) => println!("  {}: {}", key, text),
                Err(err) => println!("  {}: <{} bytes>", key, err.as_bytes().len()),
            },
            None => println!("  {}: -", key),
        }
    }

    let (layout_page, layout_page_idx) = if let Some(layout_page_idx) = zim_file.header.layout_page
    {
        let page = zim_file.get_by_url_index(layout_page_idx)?;

        (page.url, layout_page_idx as isize)
    } else {
        ("-".into(), -1)
    };

    println!(
        "Layout page: \"{}\" (index: {})",
        layout_page, layout_page_idx
    );

    Ok(())
}
