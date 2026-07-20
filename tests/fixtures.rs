//! Tests against real ZIM archives.
//!
//! The fixtures are the [openZIM testing suite](https://github.com/openzim/zim-testing-suite),
//! vendored as a submodule pinned to a release tag. It covers every format version this crate
//! reads, plus a set of deliberately corrupted archives.
//!
//! These drive the crate from outside, exactly as a consumer would, so anything they need has to
//! be part of the public API.

use std::path::{Path, PathBuf};

use testdir::testdir;
use zim::{Compression, Error, Namespace, Result, Zim};

/// Root of the vendored testing suite.
const FIXTURES: &str = "fixtures/data";

/// Format version 5.0: the old namespace scheme, and LZMA2 clusters.
const V50: &str = "withns/small.zim";
/// Format version 6.1: the new namespace scheme, still carrying a header title pointer.
const V61: &str = "nons/small.zim";
/// Format version 6.2, and by far the largest fixture - 20k entries over 18 clusters.
const V62: &str = "nons/wikipedia_en_climate_change_mini_2024-06.zim";
/// Format version 6.3: no header title pointer, and no v0 listing.
const V63: &str = "noTitleListingV0/small.zim";
/// Format version 6.3, with several clusters and non-ASCII paths.
const V63_BOOKS: &str = "noTitleListingV0/wikibooks_be_all_nopic_2017-02.zim";

/// Path to a fixture.
///
/// Say plainly that the submodule has not been checked out, rather than surfacing a bare
/// "no such file" from deep in the parser.
fn fixture(name: &str) -> PathBuf {
    let path = Path::new(FIXTURES).join(name);
    assert!(
        path.exists(),
        "missing fixture {name} - run `git submodule update --init`"
    );

    path
}

fn open(name: &str) -> Zim {
    Zim::new(fixture(name)).expect("failed to parse fixture")
}

/// Copies a blob out, for comparing against expected bytes.
fn blob(zim: &Zim, cluster: u32, idx: u32) -> Vec<u8> {
    let cluster = zim.get_cluster(cluster).unwrap();
    let guard = cluster.read().unwrap();

    guard.blob(idx).unwrap().to_vec()
}

/// Every entry an archive exposes, for comparing two ways of opening the same bytes.
fn snapshot(zim: &Zim) -> Vec<(String, String, String, Option<Vec<u8>>)> {
    zim.iterate_by_urls()
        .map(|entry| {
            let entry = entry.expect("entry must parse");
            let data = zim
                .entry_content(&entry)
                .expect("entry content must resolve")
                .map(|content| content.to_vec().expect("entry data must read"));

            (
                format!("{:?}", entry.namespace),
                entry.url.clone(),
                format!("{:?} {:?} {:?}", entry.title, entry.target, entry.mime_type),
                data,
            )
        })
        .collect()
}

/// The headline check: each supported format version opens and reports itself correctly.
#[test]
fn reads_every_supported_format_version() {
    // name, version, entries, clusters, main page path
    let cases = [
        (V50, (5, 0), 17, 2, "main.html"),
        (V61, (6, 1), 16, 2, "main.html"),
        (V62, (6, 2), 20568, 18, "index"),
        (V63, (6, 3), 16, 2, "main.html"),
        (V63_BOOKS, (6, 3), 123, 2, "Першая_старонка.html"),
    ];

    for (name, version, entries, clusters, main_page) in cases {
        let zim = open(name);

        assert_eq!(
            (zim.header.version_major, zim.header.version_minor),
            version,
            "{name}"
        );
        assert_eq!(zim.header.article_count, entries, "{name}");
        assert_eq!(zim.header.cluster_count, clusters, "{name}");

        let main = zim
            .main_page()
            .unwrap()
            .unwrap_or_else(|| panic!("{name} should have a main page"));
        assert_eq!(main.url, main_page, "{name}");

        // Every entry must parse, and the archive must match its own checksum.
        let parsed = zim
            .iterate_by_urls()
            .collect::<Result<Vec<_>>>()
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(parsed.len(), entries as usize, "{name}");

        zim.verify_checksum()
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

/// Version 5 archives spread content over the old namespace scheme, and the main page is
/// reachable only through the header - there is no `W/mainPage` well known entry.
#[test]
fn reads_the_old_namespace_scheme() {
    let zim = open(V50);

    let mut seen: Vec<char> = zim
        .iterate_by_urls()
        .map(|e| e.unwrap().namespace.as_byte() as char)
        .collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen, ['-', 'A', 'I', 'M', 'X']);

    assert!(
        zim.find_by_path(Namespace::CategoriesArticle, "mainPage")
            .unwrap()
            .is_none(),
        "version 5 has no W namespace"
    );
    let main = zim.main_page().unwrap().expect("main page via the header");
    assert_eq!(main.namespace, Namespace::Articles);
    assert_eq!(main.url, "main.html");
}

/// The only fixture using LZMA2 rather than zstd, so the only cover for that decoder.
#[test]
fn decompresses_lzma2_clusters() {
    let zim = open(V50);

    assert_eq!(
        zim.get_cluster(0).unwrap().compression(),
        Compression::Lzma2
    );

    let cluster = zim.get_cluster(0).unwrap();
    let guard = cluster.read().unwrap();
    assert_eq!(guard.blob_count(), 13);
    assert_eq!(guard.blob(0).unwrap(), b"=en");

    // The second cluster is a stored PNG, so it exercises the uncompressed path alongside.
    assert_eq!(zim.get_cluster(1).unwrap().compression(), Compression::None);
    let png = blob(&zim, 1, 0);
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
}

/// Deliberately corrupted archives from the openZIM testing suite.
///
/// A reader handed an untrusted file must reject it, not abort the process. Every one of these is
/// exercised end to end - opened, walked, and every blob read - and the test passes only by
/// reaching the end without panicking.
#[test]
fn corrupt_archives_are_rejected_without_panicking() {
    let dir = Path::new(FIXTURES).join("nons");
    let mut corrupt: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("missing fixtures - run `git submodule update --init`")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("invalid."))
        })
        .collect();
    corrupt.sort();

    assert!(
        corrupt.len() >= 20,
        "expected the corrupt fixture set, found {}",
        corrupt.len()
    );

    let mut rejected_at_open = 0usize;
    for path in &corrupt {
        let Ok(zim) = Zim::new(path) else {
            rejected_at_open += 1;
            continue;
        };

        for entry in zim.iterate_by_urls() {
            let Ok(entry) = entry else { continue };
            if let Ok(Some(content)) = zim.entry_content(&entry) {
                let _ = content.to_vec();
            }
        }

        for idx in 0..zim.header.cluster_count {
            let Ok(cluster) = zim.get_cluster(idx) else {
                continue;
            };
            let Ok(guard) = cluster.read() else { continue };
            for blob in 0..guard.blob_count() as u32 + 1 {
                let _ = guard.blob(blob);
            }
        }

        let _ = zim.main_page();
        let _ = zim.metadata_keys();
        let _ = zim.entry_list_by_title().map(|l| l.map(|l| l.to_vec()));
        let _ = zim.article_list_by_title().map(|l| l.map(|l| l.to_vec()));
        let _ = zim.fulltext_index();

        // None of these files is intact, so none may pass the checksum.
        assert!(
            zim.verify_checksum().is_err(),
            "{} passed its checksum",
            path.display()
        );
    }

    assert!(
        rejected_at_open >= 5,
        "expected the header checks to reject several archives outright, got {rejected_at_open}"
    );
}

/// Contradictory header offsets must be caught when the archive is opened. Otherwise they surface
/// much later as an out-of-bounds read against an unrelated structure, or - for the last cluster,
/// whose extent is derived from `checksumPos` - as silently wrong data.
#[test]
fn corrupt_headers_are_rejected_on_open() {
    let original = std::fs::read(fixture(V63)).expect("failed to read fixture");
    let file_len = original.len() as u64;

    let cases: [(usize, Vec<u8>, &str); 6] = [
        (
            32,
            8u64.to_le_bytes().into(),
            "pathPtrPos inside the header",
        ),
        (
            48,
            40u64.to_le_bytes().into(),
            "clusterPtrPos inside the header",
        ),
        (
            56,
            8u64.to_le_bytes().into(),
            "mimeListPos inside the header",
        ),
        (
            72,
            file_len.to_le_bytes().into(),
            "checksumPos not 16 bytes from the end",
        ),
        (
            28,
            u32::MAX.to_le_bytes().into(),
            "clusterCount exceeding entryCount",
        ),
        // Valid on its own, but the list it points at cannot fit in what is left of the file. The
        // lists are read on demand, so this has to be caught here rather than on access.
        (
            32,
            (file_len - 10).to_le_bytes().into(),
            "path pointer list running past the end",
        ),
    ];

    let dir: PathBuf = testdir!();

    for (offset, value, what) in cases {
        let mut corrupt = original.clone();
        corrupt[offset..offset + value.len()].copy_from_slice(&value);

        let path = dir.join(format!("corrupt-{offset}.zim"));
        std::fs::write(&path, &corrupt).expect("failed to write test archive");

        match Zim::new(&path) {
            Err(Error::InvalidHeader(_)) => {}
            Err(other) => panic!("{what}: expected InvalidHeader, got {other:?}"),
            Ok(_) => panic!("{what}: expected the archive to be rejected"),
        }
    }
}

/// Indices reaching the public accessors come from the archive itself - redirect targets, dirent
/// cluster numbers, the header's main page - so a corrupt file must produce an error rather than
/// abort the calling process.
#[test]
fn out_of_range_accessors_error_instead_of_panicking() {
    let zim = open(V63);

    assert!(zim.get_by_url_index(zim.header.article_count).is_err());
    assert!(zim.get_by_url_index(u32::MAX).is_err());

    assert!(zim.get_cluster(zim.header.cluster_count).is_err());
    assert!(zim.get_cluster(u32::MAX).is_err());

    let cluster = zim.get_cluster(0).unwrap();
    let guard = cluster.read().unwrap();
    assert!(guard.blob(u32::MAX).is_err());

    // The offset table carries one entry more than there are blobs. Treating that end sentinel as
    // a blob used to hand back a bogus slice instead of erroring.
    let count = guard.blob_count() as u32;
    assert!(count > 0, "cluster 0 should contain at least one blob");
    assert!(guard.blob(count - 1).is_ok(), "last blob must be readable");
    assert!(guard.blob(count).is_err(), "the end sentinel is not a blob");
}

/// `find_by_path` is a binary search, which is only sound because the format guarantees that
/// directory entries are stored ordered by namespace byte then path.
#[test]
fn entries_are_ordered_by_namespace_and_path() {
    for name in [V50, V50_BOOKS, V61, V62, V63, V63_BOOKS] {
        let zim = open(name);
        let entries = zim
            .iterate_by_urls()
            .collect::<Result<Vec<_>>>()
            .expect("failed to read entries");

        for pair in entries.windows(2) {
            let before = (pair[0].namespace.as_byte(), pair[0].url.as_str());
            let after = (pair[1].namespace.as_byte(), pair[1].url.as_str());
            assert!(before < after, "{name}: {before:?} must precede {after:?}");
        }
    }
}

#[test]
fn find_by_path_locates_well_known_entries() {
    let zim = open(V62);

    // The entries a reader needs in order to resolve anything in a 6.1+ archive.
    for (namespace, path) in [
        (Namespace::CategoriesArticle, "mainPage"),
        (Namespace::FulltextIndex, "listing/titleOrdered/v0"),
        (Namespace::FulltextIndex, "listing/titleOrdered/v1"),
        (Namespace::FulltextIndex, "fulltext/xapian"),
        (Namespace::FulltextIndex, "title/xapian"),
        (Namespace::Metadata, "Title"),
        (Namespace::Metadata, "Illustration_48x48@1"),
    ] {
        let found = zim.find_by_path(namespace, path).unwrap();
        assert!(found.is_some(), "{namespace:?}/{path} should be found");

        let entry = zim.get_by_path(namespace, path).unwrap().unwrap();
        assert_eq!(entry.url, path);
        assert_eq!(entry.namespace, namespace);
    }

    assert_eq!(
        zim.find_by_path(Namespace::UserContent, "definitely/not/here")
            .unwrap(),
        None
    );
    // Present as a path, but only under a different namespace.
    assert_eq!(
        zim.find_by_path(Namespace::UserContent, "mainPage")
            .unwrap(),
        None
    );
}

/// Every entry must be findable at exactly its own index, which catches comparator mistakes that
/// spot checks on a handful of paths would miss.
#[test]
fn find_by_path_agrees_with_iteration() {
    for name in [V50, V50_BOOKS, V63, V63_BOOKS] {
        let zim = open(name);

        for (idx, entry) in zim.iterate_by_urls().enumerate() {
            let entry = entry.expect("failed to read entry");
            assert_eq!(
                zim.find_by_path(entry.namespace, &entry.url).unwrap(),
                Some(idx as u32),
                "{name}: {:?}/{}",
                entry.namespace,
                entry.url
            );
        }
    }
}

#[test]
fn namespace_range_covers_exactly_one_namespace() {
    let zim = open(V62);

    let range = zim.namespace_range(Namespace::Metadata).unwrap();
    assert!(!range.is_empty());

    for idx in range.clone() {
        assert_eq!(
            zim.get_by_url_index(idx).unwrap().namespace,
            Namespace::Metadata
        );
    }

    // The neighbours must fall outside it.
    if range.start > 0 {
        assert_ne!(
            zim.get_by_url_index(range.start - 1).unwrap().namespace,
            Namespace::Metadata
        );
    }
    assert_ne!(
        zim.get_by_url_index(range.end).unwrap().namespace,
        Namespace::Metadata
    );
}

#[test]
fn reads_metadata() {
    let zim = open(V62);

    let title = zim
        .metadata("Title")
        .unwrap()
        .expect("Title is mandatory")
        .to_vec()
        .unwrap();
    assert_eq!(
        String::from_utf8(title).unwrap(),
        "Climate change by Wikipedia"
    );

    assert!(zim.metadata("NoSuchMetadataEntry").unwrap().is_none());

    let keys = zim.metadata_keys().unwrap();
    for expected in [
        "Title",
        "Language",
        "Creator",
        "Date",
        "Illustration_48x48@1",
    ] {
        assert!(keys.contains(&expected.to_string()), "missing {expected}");
    }
    // Namespace scanning must stop at the namespace boundary rather than run to the end.
    assert!(keys.len() < zim.header.article_count as usize);
}

#[test]
fn exposes_xapian_indexes() {
    let zim = open(V62);

    // Sizes are read through the handle rather than by loading the index: on a full archive the
    // fulltext index runs to gigabytes.
    let fulltext = zim.fulltext_index().unwrap().expect("fulltext index");
    assert_eq!(fulltext.len().unwrap(), 1_646_592);

    let title = zim.title_index().unwrap().expect("title index");
    assert_eq!(title.len().unwrap(), 827_392);

    // Absent indexes are reported as such rather than erroring.
    assert!(open(V63).fulltext_index().unwrap().is_none());
}

/// Asking a cluster about itself must not pull its data in. Clusters are not small, so a reader
/// that inspects every cluster, as `zim-info` does, would otherwise fault in most of the archive.
#[test]
fn cluster_data_loads_only_on_blob_access() {
    let zim = open(V62);

    for idx in 0..zim.header.cluster_count {
        let cluster = zim.get_cluster(idx).unwrap();
        let _ = cluster.compression();
        assert!(
            !cluster.is_loaded(),
            "cluster {idx} was loaded just to report its compression"
        );
    }

    let cluster = zim.get_cluster(0).unwrap();
    let guard = cluster.read().unwrap();
    assert!(guard.blob_count() > 0);
    assert!(cluster.is_loaded(), "reading a blob must load the cluster");
}

/// Content from an uncompressed cluster must be borrowed straight from the mapping rather than
/// copied. This is what keeps a multi-gigabyte search index from having to be read into memory in
/// order to be used, and the format requires indexes and listings to be stored uncompressed
/// precisely so that this is possible.
#[test]
fn index_content_is_borrowed_from_the_mapping() {
    let zim = open(V62);

    for content in [
        zim.fulltext_index().unwrap().expect("fulltext index"),
        zim.title_index().unwrap().expect("title index"),
    ] {
        let borrowed = content
            .with(|bytes| {
                let start = bytes.as_ptr() as usize;

                zim.store
                    .prefix_chunks(zim.store.len())
                    .iter()
                    .any(|chunk| {
                        let low = chunk.as_ptr() as usize;
                        start >= low && start < low + chunk.len()
                    })
            })
            .unwrap();

        assert!(borrowed, "content must point into the mapping, not a copy");
    }
}

/// Version 6.3 removed both the header's title pointer list and the `v0` listing, leaving only the
/// article listing. Title ordering must still be reachable for such archives.
#[test]
fn title_listings_track_the_format_version() {
    for name in [V50, V61, V62] {
        let zim = open(name);
        assert!(
            zim.header.title_ptr_pos.is_some(),
            "{name} predates 6.3 and should carry titlePtrPos"
        );
        assert!(
            zim.entry_list_by_title().unwrap().is_some(),
            "{name} should expose an all-entry title listing"
        );
    }

    for name in [V63, V63_BOOKS] {
        let zim = open(name);
        assert!(zim.header.title_ptr_pos.is_none(), "{name}");
        assert!(
            zim.entry_list_by_title().unwrap().is_none(),
            "{name} dropped both the v0 listing and titlePtrPos"
        );

        let articles = zim
            .article_list_by_title()
            .unwrap()
            .expect("6.3 archives carry the v1 listing");
        assert!(!articles.is_empty().unwrap(), "{name}");
        assert!(
            articles.len().unwrap() <= zim.header.article_count as usize,
            "{name}"
        );

        // The listing must decode to real indices, in title order - a wrong element width or
        // endianness would still produce a plausible-looking list of numbers.
        let mut titles: Vec<String> = Vec::new();
        articles
            .for_each(|idx| {
                let entry = zim
                    .get_by_url_index(idx)
                    .expect("listing index must be valid");
                titles.push(if entry.title.is_empty() {
                    entry.url
                } else {
                    entry.title
                });
            })
            .unwrap();
        assert!(
            titles.windows(2).all(|pair| pair[0] <= pair[1]),
            "{name}: v1 listing is not in title order"
        );
    }
}

/// The spec says `titlePtrPos` points directly at the `v0` listing entry's data. Check that
/// against the archive rather than trusting it, since the header pointer is the fallback used
/// when the listing entry is absent.
#[test]
fn title_pointer_addresses_the_v0_listing_data() {
    let zim = open(V61);

    let via_entry = zim
        .entry_list_by_title()
        .unwrap()
        .expect("6.1 archives carry the v0 listing")
        .to_vec()
        .unwrap();
    assert_eq!(via_entry.len(), zim.header.article_count as usize);

    let pos = zim
        .header
        .title_ptr_pos
        .expect("6.1 archives carry titlePtrPos");
    let raw = zim
        .store
        .slice(pos, via_entry.len() as u64 * 4)
        .expect("titlePtrPos must address readable bytes");
    let via_header: Vec<u32> = raw
        .chunks_exact(4)
        .map(|raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
        .collect();

    assert_eq!(via_header, via_entry);
}

/// Writes `raw` out as chunk files named `archive.zimaa`, `archive.zimab`, ... in `dir`, and
/// returns the path of the archive as a whole - which deliberately does not exist.
fn write_chunks(dir: &Path, raw: &[u8], chunk_size: usize) -> PathBuf {
    std::fs::create_dir_all(dir).expect("failed to create chunk dir");

    for (idx, chunk) in raw.chunks(chunk_size).enumerate() {
        let suffix = format!(
            "{}{}",
            (b'a' + (idx / 26) as u8) as char,
            (b'a' + (idx % 26) as u8) as char
        );
        std::fs::write(dir.join(format!("archive.zim{suffix}")), chunk)
            .expect("failed to write chunk");
    }

    dir.join("archive.zim")
}

/// A split archive is the concatenation of its chunks, so it must read exactly like the whole
/// file - including for structures that straddle a chunk boundary and therefore have no
/// contiguous backing to borrow from.
#[test]
fn reads_a_split_archive_identically_to_the_whole_file() {
    let single = open(V63_BOOKS);
    single
        .verify_checksum()
        .expect("fixture checksum must match");
    let expected = snapshot(&single);

    let raw = std::fs::read(fixture(V63_BOOKS)).expect("failed to read fixture");
    let root: PathBuf = testdir!();

    // Boundaries chosen to land in the directory entries, the clusters, the pointer lists, and
    // inside the trailing checksum respectively. The 26x26 naming scheme caps the chunk count at
    // 676, so none of these may be too small.
    for chunk_size in [1_000, 40_000, raw.len() / 2, raw.len() - 20, raw.len() - 1] {
        let dir = root.join(format!("split-{chunk_size}"));
        let base = write_chunks(&dir, &raw, chunk_size);
        assert!(!base.exists(), "the archive itself must not exist");

        let split = Zim::new(&base).expect("failed to open split archive");
        assert_eq!(
            split.store.len(),
            raw.len() as u64,
            "chunk size {chunk_size}"
        );
        assert_eq!(
            split.header.article_count, single.header.article_count,
            "chunk size {chunk_size}"
        );

        let actual = snapshot(&split);
        assert_eq!(actual.len(), expected.len(), "chunk size {chunk_size}");
        for (idx, (got, want)) in actual.iter().zip(expected.iter()).enumerate() {
            assert_eq!(got, want, "chunk size {chunk_size}, entry {idx}");
        }

        // The checksum covers the archive as a whole, not any single chunk.
        split
            .verify_checksum()
            .unwrap_or_else(|e| panic!("chunk size {chunk_size}: checksum: {e}"));

        // Naming the first chunk must resolve to the same archive.
        let via_first_chunk =
            Zim::new(dir.join("archive.zimaa")).expect("failed to open via first chunk");
        assert_eq!(via_first_chunk.store.len(), raw.len() as u64);
    }
}

/// A whole archive takes precedence over chunks sharing its name, and a chunk sequence stops at
/// the first gap rather than skipping over it.
#[test]
fn chunk_discovery_prefers_the_whole_archive_and_stops_at_a_gap() {
    let raw = std::fs::read(fixture(V63)).expect("failed to read fixture");

    let dir: PathBuf = testdir!();
    let base = write_chunks(&dir, &raw, raw.len() / 3);

    // Dropping the second chunk truncates the archive, which must fail the header's "checksum
    // sits 16 bytes before the end" invariant rather than read short.
    std::fs::remove_file(dir.join("archive.zimab")).unwrap();
    assert!(Zim::new(&base).is_err(), "a gap must not be skipped over");

    // With the whole archive present under the same name, the chunks are ignored.
    std::fs::write(&base, &raw).unwrap();
    let whole = Zim::new(&base).expect("failed to open whole archive");
    assert_eq!(whole.store.len(), raw.len() as u64);
}

/// Format version 5.0, multi-cluster: the only legacy-namespace archive with enough entries to
/// put a namespace boundary in the middle of a binary search.
const V50_BOOKS: &str = "withns/wikibooks_be_all_nopic_2017-02.zim";

/// What a deliberately corrupted archive is expected to do.
///
/// The point is the *tier*, not just "it fails somewhere": a lazy failure silently becoming an
/// open failure, or vice versa, is what breaks real archives, and a blanket "does not panic" loop
/// cannot see that.
#[derive(Debug, PartialEq)]
enum Fails {
    /// Rejected by `Zim::new`.
    AtOpen,
    /// Opens, but this entry index fails to parse.
    AtEntry(usize),
    /// Opens and every entry parses, but this cluster cannot be read.
    AtCluster(u32),
    /// Opens and reads clean - we have no validator for this corruption.
    Never,
}

/// Classifies an archive by where it first fails.
fn failure_tier(path: &Path) -> Fails {
    let Ok(zim) = Zim::new(path) else {
        return Fails::AtOpen;
    };

    for (idx, entry) in zim.iterate_by_urls().enumerate() {
        if entry.is_err() {
            return Fails::AtEntry(idx);
        }
    }

    for idx in 0..zim.header.cluster_count {
        if zim
            .get_cluster(idx)
            .and_then(|c| c.read().map(|_| ()))
            .is_err()
        {
            return Fails::AtCluster(idx);
        }
    }

    Fails::Never
}

/// Every corrupted archive in the suite, across all three variant builds, pinned to the exact
/// point at which it fails.
#[test]
fn corrupt_archives_fail_at_the_expected_point() {
    for dir in ["withns", "nons", "noTitleListingV0"] {
        let root = Path::new(FIXTURES).join(dir);
        // `outofbounds_last_direntptr` breaks the final entry, whose index differs per variant.
        let last = Zim::new(root.join("small.zim"))
            .expect("variant must ship a good small.zim")
            .header
            .article_count as usize
            - 1;

        let mut seen = 0usize;
        for entry in std::fs::read_dir(&root).expect("missing fixtures") {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap().to_string();
            let Some(kind) = name
                .strip_prefix("invalid.")
                .and_then(|n| n.strip_suffix(".zim"))
            else {
                continue;
            };
            seen += 1;

            let expected = match kind {
                "smaller_than_header"
                | "invalid_mimelistpos"
                | "invalid_checksumpos"
                | "outofbounds_urlptrpos"
                | "outofbounds_clusterptrpos"
                | "bad_mimetype_list" => Fails::AtOpen,

                "outofbounds_first_direntptr" => Fails::AtEntry(0),
                "outofbounds_last_direntptr" => Fails::AtEntry(last),
                "bad_mimetype_in_dirent" => Fails::AtEntry(8),

                "outofbounds_first_clusterptr" => Fails::AtCluster(0),
                "offset_in_cluster"
                | "too_large_offset_of_first_blob_in_cluster"
                | "too_small_offset_of_first_blob_in_cluster_0"
                | "too_small_offset_of_first_blob_in_cluster_7"
                | "misaligned_offset_of_first_blob_in_cluster_9"
                | "misaligned_offset_of_first_blob_in_cluster_10"
                | "misaligned_offset_of_first_blob_in_cluster_11" => Fails::AtCluster(1),

                // Corruptions we deliberately do not detect. Pinned so a change is visible:
                // the dirent/title tables are not validated for ordering, an out-of-range title
                // index is only caught when used, and a stale deprecated titlePtrPos must not
                // make an otherwise readable archive unopenable.
                "nonsorted_dirent_table"
                | "nonsorted_title_index"
                | "outofbounds_first_title_entry"
                | "outofbounds_last_title_entry"
                | "outofbounds_titleptrpos"
                | "too_small_offset_of_first_blob_in_cluster_4" => Fails::Never,

                other => panic!("unclassified corrupt fixture {dir}/{other} - add it to the table"),
            };

            assert_eq!(failure_tier(&path), expected, "{dir}/{name}");
        }

        assert!(seen >= 19, "{dir}: only {seen} corrupt fixtures found");
    }
}

/// One unreadable entry must not cost the rest of the archive. Iteration reports it and carries
/// on, and its neighbours stay reachable by index.
#[test]
fn a_single_bad_entry_does_not_poison_the_archive() {
    let path = Path::new(FIXTURES).join("nons/invalid.bad_mimetype_in_dirent.zim");
    let zim = Zim::new(&path).expect("only one dirent is corrupt");

    let results: Vec<_> = zim.iterate_by_urls().collect();
    assert_eq!(results.len(), zim.header.article_count as usize);
    assert_eq!(
        results.iter().filter(|r| r.is_err()).count(),
        1,
        "exactly one entry should fail"
    );
    assert!(results[8].is_err());

    // Its neighbours are unaffected.
    assert!(zim.get_by_url_index(7).is_ok());
    assert!(zim.get_by_url_index(9).is_ok());
}

/// Listing entries hold entry indices straight off disk. Feeding one back in must be bounds
/// checked - this is the step that turns a corrupt file into an out-of-range access.
#[test]
fn listing_indices_are_treated_as_untrusted() {
    for name in [
        "nons/invalid.outofbounds_first_title_entry.zim",
        "nons/invalid.outofbounds_last_title_entry.zim",
        "nons/invalid.nonsorted_title_index.zim",
    ] {
        let zim = Zim::new(Path::new(FIXTURES).join(name)).expect("these open clean");

        let Some(listing) = zim.entry_list_by_title().unwrap() else {
            continue;
        };

        let mut out_of_range = 0usize;
        listing
            .for_each(|idx| {
                if zim.get_by_url_index(idx).is_err() {
                    out_of_range += 1;
                }
            })
            .unwrap();

        // Whatever the listing claims, nothing may panic and the archive stays usable.
        assert!(zim.get_by_url_index(0).is_ok(), "{name}");
        let _ = out_of_range;
    }
}

/// Redirects are a third of the entries in a real archive, and `resolve` is public API, yet the
/// redirect branch of dirent parsing reads its target from the same field offset that holds the
/// cluster number for content entries - a regression there yields a plausible wrong index.
#[test]
fn resolves_redirects_to_their_target() {
    let zim = open(V63_BOOKS);

    let redirects: Vec<_> = zim
        .iterate_by_urls()
        .map(|e| e.unwrap())
        .filter(|e| e.mime_type == zim::MimeType::Redirect)
        .collect();
    assert_eq!(redirects.len(), 6);

    for entry in redirects {
        let Some(zim::Target::Redirect(target)) = entry.target else {
            panic!("a redirect must carry a redirect target");
        };
        assert!(target < zim.header.article_count, "target in range");

        // A redirect stores no content of its own.
        assert!(zim.entry_content(&entry).unwrap().is_none());

        let url = entry.url.clone();
        let resolved = zim.resolve(entry).expect("redirect must resolve");
        assert_ne!(
            resolved.mime_type,
            zim::MimeType::Redirect,
            "{url} resolved to another redirect"
        );
        assert_eq!(
            resolved.url,
            zim.get_by_url_index(target).unwrap().url,
            "{url} resolved somewhere other than its target"
        );
        assert!(zim.entry_content(&resolved).unwrap().is_some());
    }
}

/// Overwrites the redirect target of entry `idx`, and returns the patched archive's path.
///
/// A redirect dirent is mimetype(2) parameterLen(1) namespace(1) revision(4) target(4), so the
/// target sits at offset 8.
fn patch_redirect_target(dir: &Path, source: &str, idx: u32, target: u32) -> PathBuf {
    let mut raw = std::fs::read(fixture(source)).expect("failed to read fixture");

    let url_ptr_pos = u64::from_le_bytes(raw[32..40].try_into().unwrap()) as usize;
    let at = url_ptr_pos + idx as usize * 8;
    let dirent = u64::from_le_bytes(raw[at..at + 8].try_into().unwrap()) as usize;

    assert_eq!(
        u16::from_le_bytes(raw[dirent..dirent + 2].try_into().unwrap()),
        0xffff,
        "entry {idx} of {source} must be a redirect"
    );
    raw[dirent + 8..dirent + 12].copy_from_slice(&target.to_le_bytes());

    let path = dir.join(format!("redirect-{target}.zim"));
    std::fs::write(&path, &raw).expect("failed to write patched archive");

    path
}

/// A redirect may legally point at another redirect, so resolution follows the chain - which
/// means a cycle has to be bounded. A self-redirect is the cheapest denial of service against a
/// reader, and no fixture contains one.
#[test]
fn redirect_cycles_and_bad_targets_are_rejected() {
    let dir: PathBuf = testdir!();

    // Entry 12 of nons/small.zim is W/mainPage, a redirect to entry 1.
    let looping = patch_redirect_target(&dir, V61, 12, 12);
    let zim = Zim::new(&looping).expect("still a structurally valid archive");
    let entry = zim.get_by_url_index(12).unwrap();
    assert!(
        matches!(zim.resolve(entry), Err(Error::RedirectLoop)),
        "a self-redirect must be caught, not followed forever"
    );

    let dangling = patch_redirect_target(&dir, V61, 12, u32::MAX);
    let zim = Zim::new(&dangling).expect("still a structurally valid archive");
    let entry = zim.get_by_url_index(12).unwrap();
    assert!(
        matches!(zim.resolve(entry), Err(Error::OutOfBounds)),
        "a target past the end must error, not panic"
    );
}

/// The suite ships archives already split by `zimsplit`, cut between clusters as the format
/// requires and into wildly unequal parts. Our own chunking test uses uniform parts, so this is
/// the only cover for a reader that assumes a fixed part stride.
#[test]
fn reads_the_suite_pre_split_archives() {
    for dir in ["withns", "nons", "noTitleListingV0"] {
        let whole = Zim::new(
            Path::new(FIXTURES)
                .join(dir)
                .join("wikibooks_be_all_nopic_2017-02.zim"),
        )
        .expect("failed to open the single-file archive");

        let base = Path::new(FIXTURES)
            .join(dir)
            .join("wikibooks_be_all_nopic_2017-02_splitted.zim");
        assert!(!base.exists(), "the split archive has no whole-file form");

        let split = Zim::new(&base).unwrap_or_else(|e| panic!("{dir}: {e}"));

        assert_eq!(split.store.len(), whole.store.len(), "{dir}");
        assert_eq!(
            split.header.article_count, whole.header.article_count,
            "{dir}"
        );
        assert_eq!(snapshot(&split), snapshot(&whole), "{dir}");
        split
            .verify_checksum()
            .unwrap_or_else(|e| panic!("{dir}: {e}"));

        // Naming the first chunk resolves the same archive.
        let via_chunk =
            Zim::new(base.with_extension("zimaa")).unwrap_or_else(|e| panic!("{dir}: {e}"));
        assert_eq!(via_chunk.store.len(), whole.store.len(), "{dir}");
    }
}

/// A `.zimaa` name always means the chunked archive. If that rule were dropped, the name would
/// silently open a single-file archive of the same stem - a wrong-file bug producing a perfectly
/// valid result that no other assertion could catch.
#[test]
fn a_first_chunk_name_never_opens_a_whole_archive() {
    // This archive exists only in single-file form, so its `.zimaa` name must not resolve.
    let single = fixture("nons/wikibooks_be_all_nopic_2017-02.zim");
    assert!(single.exists());

    assert!(
        Zim::new(single.with_extension("zimaa")).is_err(),
        "a .zimaa name must not fall back to the whole archive"
    );
}

/// Reads every blob of every fixture. Covers the last cluster's extent, which is derived from
/// `checksumPos` rather than the pointer table, and both decoders, in one pass.
#[test]
fn reads_every_blob_of_every_fixture() {
    // name, total blobs across all clusters
    for (name, total) in [
        (V50, 16),
        (V50_BOOKS, 113),
        (V61, 15),
        (V62, 4001),
        (V63, 15),
        (V63_BOOKS, 117),
    ] {
        let zim = open(name);

        let mut blobs = 0usize;
        let mut bytes = 0usize;
        for idx in 0..zim.header.cluster_count {
            let cluster = zim
                .get_cluster(idx)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let guard = cluster
                .read()
                .unwrap_or_else(|e| panic!("{name} cluster {idx}: {e}"));

            for blob in 0..guard.blob_count() as u32 {
                bytes += guard
                    .blob(blob)
                    .unwrap_or_else(|e| panic!("{name} cluster {idx} blob {blob}: {e}"))
                    .len();
            }
            blobs += guard.blob_count();

            // One past the last blob is the offset table's end sentinel, not a blob.
            assert!(guard.blob(guard.blob_count() as u32).is_err(), "{name}");
        }

        assert_eq!(blobs, total, "{name}");
        assert!(bytes > 0, "{name}");
    }
}

/// The MIME list sits immediately after the header, so `mimeListPos` is the header size and is
/// always 80. Accepting any other value reads the list out of the middle of another structure and
/// yields plausible but wrong types for every entry, which is far worse than refusing the file.
#[test]
fn mime_list_pos_is_pinned_to_the_header_size() {
    let original = std::fs::read(fixture(V61)).expect("failed to read fixture");
    let dir: PathBuf = testdir!();

    for pos in [0u64, 72, 79, 81, 96, 255] {
        let mut corrupt = original.clone();
        corrupt[56..64].copy_from_slice(&pos.to_le_bytes());

        let path = dir.join(format!("mimelistpos-{pos}.zim"));
        std::fs::write(&path, &corrupt).unwrap();

        assert!(
            matches!(Zim::new(&path), Err(Error::InvalidHeader(_))),
            "mimeListPos {pos} should be rejected"
        );
    }
}

/// Truncated files, empty files, and valid magic over a garbage body must all be refused rather
/// than opened or panicked on.
#[test]
fn garbage_input_is_refused() {
    let dir: PathBuf = testdir!();
    let magic = 72_173_914u32.to_le_bytes();

    for with_magic in [false, true] {
        for fill in [0x00u8, 0x01, 0x11, 0x30, 0xff] {
            for len in [0usize, 4, 8, 40, 79, 80, 88, 200] {
                let mut raw = vec![fill; len];
                if with_magic && len >= 4 {
                    raw[..4].copy_from_slice(&magic);
                }

                let path = dir.join(format!("garbage-{with_magic}-{fill}-{len}.zim"));
                std::fs::write(&path, &raw).unwrap();

                assert!(
                    Zim::new(&path).is_err(),
                    "magic={with_magic} fill={fill:#x} len={len} should be refused"
                );
            }
        }
    }
}

/// A failing checksum must not stop an archive being read. libzim treats verification as a
/// separate step, and refusing to open would make us reject archives it reads happily.
#[test]
fn a_bad_checksum_does_not_prevent_reading() {
    let mut raw = std::fs::read(fixture(V63)).expect("failed to read fixture");
    let len = raw.len();
    raw[len - 8] ^= 0xff; // inside the trailing MD5

    let dir: PathBuf = testdir!();
    let path = dir.join("bad-checksum.zim");
    std::fs::write(&path, &raw).unwrap();

    let zim = Zim::new(&path).expect("a bad checksum must not prevent opening");
    assert!(matches!(zim.verify_checksum(), Err(Error::InvalidChecksum)));

    // and the content is still fully readable
    let entries = zim.iterate_by_urls().collect::<Result<Vec<_>>>().unwrap();
    assert_eq!(entries.len(), zim.header.article_count as usize);
    assert!(zim.main_page().unwrap().is_some());
}
