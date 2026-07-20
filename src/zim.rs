use std::cmp::Ordering;
use std::fs::File;
use std::io::Cursor;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use byteorder::{LittleEndian, ReadBytesExt};
use md5::{digest::array::Array, digest::OutputSizeUser, Digest, Md5};
use memmap::Mmap;

use crate::cluster::Cluster;
use crate::directory_entry::DirectoryEntry;
use crate::directory_iterator::DirectoryIterator;
use crate::errors::{Error, Result};
use crate::mime_type::MimeType;
use crate::namespace::Namespace;
use crate::uuid::Uuid;

/// Magic number to recognise the file format, must be 72173914
pub const ZIM_MAGIC_NUMBER: u32 = 72173914;

/// Size of the header, up to and including `checksumPos`.
///
/// The MIME type list directly follows the header, so `mimeListPos` also defines the header's
/// size and is never smaller than this.
const HEADER_SIZE: u64 = 80;

/// Represents a ZIM file
#[allow(dead_code)]
pub struct Zim {
    // Zim structure data:
    pub header: ZimHeader,

    pub master_view: Mmap,
    /// The path to the file.
    pub file_path: PathBuf,

    /// List of mimetypes used in this ZIM archive
    pub mime_table: Vec<String>, // a list of mimetypes
    pub url_list: Vec<u64>,             // a list of offsets
    pub article_list: Option<Vec<u32>>, // a list of indicies into url_list
    pub cluster_list: Vec<u64>,         // a list of offsets

    /// MD5 checksum.
    pub checksum: Checksum,
}

pub type Checksum = Array<u8, <Md5 as OutputSizeUser>::OutputSize>;

/// A ZIM file starts with a header.
#[derive(Debug)]
pub struct ZimHeader {
    /// Major version, either 5 or 6
    pub version_major: u16,
    /// Minor version
    pub version_minor: u16,
    /// unique id of this zim file
    pub uuid: Uuid,
    /// total number of articles
    pub article_count: u32,
    /// total number of clusters
    pub cluster_count: u32,
    /// position of the directory pointerlist ordered by URL
    pub url_ptr_pos: u64,
    /// position of the directory pointerlist ordered by Title
    /// Deprecated in newer versions. Use `X/listing/titleordered/v0` instead.
    pub title_ptr_pos: Option<u64>,
    /// position of the cluster pointer list
    pub cluster_ptr_pos: u64,
    /// position of the MIME type list (also header size)
    pub mime_list_pos: u64,
    /// main page or 0xffffffff if no main page
    pub main_page: Option<u32>,
    /// ayout page or 0xffffffffff if no layout page
    pub layout_page: Option<u32>,
    /// pointer to the md5checksum of this file without the checksum itself.
    /// This points always 16 bytes before the end of the file.
    pub checksum_pos: u64,
}

impl ZimHeader {
    /// Rejects headers whose offsets cannot describe a readable archive.
    ///
    /// Mirrors libzim's own header validation. Without it, contradictory offsets are only
    /// discovered much later, as an out-of-bounds read against an unrelated structure.
    fn sanity_check(&self, file_len: u64) -> Result<()> {
        if self.mime_list_pos < HEADER_SIZE {
            return Err(Error::InvalidHeader("mimeListPos overlaps the header"));
        }
        if self.mime_list_pos > file_len {
            return Err(Error::InvalidHeader("mimeListPos is past the end of file"));
        }

        // Every other structure is stored after the MIME type list.
        for (pos, what) in [
            (self.url_ptr_pos, "pathPtrPos precedes mimeListPos"),
            (
                self.title_ptr_pos.unwrap_or(u64::MAX),
                "titlePtrPos precedes mimeListPos",
            ),
            (self.cluster_ptr_pos, "clusterPtrPos precedes mimeListPos"),
            (self.checksum_pos, "checksumPos precedes mimeListPos"),
        ] {
            if pos < self.mime_list_pos {
                return Err(Error::InvalidHeader(what));
            }
        }

        // The checksum is always the final 16 bytes; the last cluster's extent is derived from
        // this, so a wrong value silently mis-sizes it.
        if self.checksum_pos.checked_add(16) != Some(file_len) {
            return Err(Error::InvalidHeader(
                "checksumPos is not 16 bytes before the end of file",
            ));
        }

        if (self.article_count == 0) != (self.cluster_count == 0) {
            return Err(Error::InvalidHeader(
                "entryCount and clusterCount disagree about being empty",
            ));
        }
        if self.cluster_count > self.article_count {
            return Err(Error::InvalidHeader("clusterCount exceeds entryCount"));
        }

        Ok(())
    }
}

impl Zim {
    /// Loads a Zim file
    ///
    /// Loads a Zim file and parses the header, and the url, title, and cluster offset tables.  The
    /// rest of the data isn't parsed until it's needed, so this should be fairly quick.
    pub fn new<P: AsRef<Path>>(p: P) -> Result<Zim> {
        let f = File::open(p.as_ref())?;
        let master_view = unsafe { Mmap::map(&f)? };

        let (header, mime_table) = parse_header(&master_view)?;
        let url_list = parse_url_list(&master_view, header.url_ptr_pos, header.article_count)?;
        let article_list = if let Some(title_ptr_pos) = header.title_ptr_pos {
            let list = parse_article_list(&master_view, title_ptr_pos, header.article_count)?;
            Some(list)
        } else {
            None
        };

        let cluster_list =
            parse_cluster_list(&master_view, header.cluster_ptr_pos, header.cluster_count)?;

        let checksum = read_checksum(&master_view, header.checksum_pos)?;

        Ok(Zim {
            header,
            file_path: p.as_ref().into(),
            master_view,
            mime_table,
            url_list,
            article_list,
            cluster_list,
            checksum,
        })
    }

    /// Computes the checksum, and returns an error if it does not match the one in
    /// the file.
    pub fn verify_checksum(&self) -> Result<()> {
        let checksum_computed = compute_checksum(&self.file_path, self.header.checksum_pos)?;

        if self.checksum != checksum_computed {
            return Err(Error::InvalidChecksum);
        }

        Ok(())
    }

    /// Indexes into the ZIM mime_table.
    pub fn get_mimetype(&self, id: u16) -> Option<MimeType> {
        match id {
            0xffff => Some(MimeType::Redirect),
            0xfffe => Some(MimeType::LinkTarget),
            0xfffd => Some(MimeType::DeletedEntry),
            id => {
                if (id as usize) < self.mime_table.len() {
                    Some(MimeType::Type(self.mime_table[id as usize].clone()))
                } else {
                    println!("WARNING unknown mimetype idx {}", id);
                    None
                }
            }
        }
    }

    /// Iterates over articles, sorted by URL.
    ///
    /// For performance reasons, you might want to extract by cluster instead.
    pub fn iterate_by_urls(&self) -> DirectoryIterator<'_> {
        DirectoryIterator::new(self)
    }

    /// Returns the `DirectoryEntry` for the article found at the given URL index.
    ///
    /// idx must be between 0 and `article_count`
    pub fn get_by_url_index(&self, idx: u32) -> Result<DirectoryEntry> {
        let entry_offset = *self.url_list.get(idx as usize).ok_or(Error::OutOfBounds)?;
        let dir_view = self
            .master_view
            .get(usize::try_from(entry_offset)?..)
            .ok_or(Error::OutOfBounds)?;

        DirectoryEntry::new(self, dir_view)
    }

    /// Returns the index of the entry at `namespace`/`path`, if it exists.
    ///
    /// Directory entries are stored ordered by their full path - the namespace byte followed by
    /// the path, compared as UTF-8 bytes - so this is a binary search over the path pointer list
    /// rather than a scan.
    ///
    /// Note that in the new namespace scheme (format version 6.1 and later) the stored path does
    /// not include the namespace, which is why it is passed separately.
    pub fn find_by_path(&self, namespace: Namespace, path: &str) -> Result<Option<u32>> {
        let target = (namespace.as_byte(), path.as_bytes());

        let mut low = 0usize;
        let mut high = self.url_list.len();

        while low < high {
            let mid = low + (high - low) / 2;
            let entry = self.get_by_url_index(mid as u32)?;

            match (entry.namespace.as_byte(), entry.url.as_bytes()).cmp(&target) {
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
                Ordering::Equal => return Ok(Some(mid as u32)),
            }
        }

        Ok(None)
    }

    /// Returns the entry at `namespace`/`path`, if it exists.
    pub fn get_by_path(&self, namespace: Namespace, path: &str) -> Result<Option<DirectoryEntry>> {
        match self.find_by_path(namespace, path)? {
            Some(idx) => Ok(Some(self.get_by_url_index(idx)?)),
            None => Ok(None),
        }
    }

    /// Returns the given `Cluster`
    ///
    /// idx must be between 0 and `cluster_count`
    pub fn get_cluster(&self, idx: u32) -> Result<Cluster<'_>> {
        Cluster::new(
            &self.master_view,
            &self.cluster_list,
            idx,
            self.header.checksum_pos,
            self.header.version_major,
        )
    }
}

fn is_defined(val: u32) -> Option<u32> {
    if val == 0xffffffff {
        None
    } else {
        Some(val)
    }
}

fn parse_header(master_view: &Mmap) -> Result<(ZimHeader, Vec<String>)> {
    let file_len = master_view.len() as u64;
    if file_len < HEADER_SIZE {
        return Err(Error::InvalidHeader("file is smaller than the header"));
    }

    let mut header_cur = Cursor::new(master_view);

    let magic = header_cur.read_u32::<LittleEndian>()?;

    if magic != ZIM_MAGIC_NUMBER {
        return Err(Error::InvalidMagicNumber);
    }

    let version_major = header_cur.read_u16::<LittleEndian>()?;
    if version_major != 5 && version_major != 6 {
        return Err(Error::InvalidVersion(version_major));
    }

    let version_minor = header_cur.read_u16::<LittleEndian>()?;

    let mut uuid = [0u8; 16];
    for el in &mut uuid {
        *el = header_cur.read_u8()?;
    }

    let article_count = header_cur.read_u32::<LittleEndian>()?;
    let cluster_count = header_cur.read_u32::<LittleEndian>()?;
    let url_ptr_pos = header_cur.read_u64::<LittleEndian>()?;
    let title_ptr_pos = header_cur.read_u64::<LittleEndian>()?;

    // Deprecated, and considered optional in newer versions
    let title_ptr_pos = if title_ptr_pos == u64::MAX {
        None
    } else {
        Some(title_ptr_pos)
    };

    let cluster_ptr_pos = header_cur.read_u64::<LittleEndian>()?;
    let mime_list_pos = header_cur.read_u64::<LittleEndian>()?;

    let main_page = header_cur.read_u32::<LittleEndian>()?;
    let layout_page = header_cur.read_u32::<LittleEndian>()?;
    let checksum_pos = header_cur.read_u64::<LittleEndian>()?;

    debug_assert_eq!(header_cur.position(), HEADER_SIZE);

    let header = ZimHeader {
        version_major,
        version_minor,
        uuid: Uuid::new(uuid),
        article_count,
        cluster_count,
        url_ptr_pos,
        title_ptr_pos,
        cluster_ptr_pos,
        mime_list_pos,
        main_page: is_defined(main_page),
        layout_page: is_defined(layout_page),
        checksum_pos,
    };
    header.sanity_check(file_len)?;

    // The MIME type list directly follows the header, so seek to `mime_list_pos` rather than
    // assuming the length of the fields read above. A header extended by a future minor version
    // is then skipped instead of being parsed as MIME types.
    header_cur.set_position(mime_list_pos);

    let mime_table = {
        let mut mime_table = Vec::new();
        loop {
            let mut mime_buf = Vec::new();
            if let Ok(size) = header_cur.read_until(0, &mut mime_buf) {
                if size <= 1 {
                    break;
                }
                mime_buf.truncate(size - 1);
                mime_table.push(String::from_utf8(mime_buf)?);
            }
        }
        mime_table
    };

    Ok((header, mime_table))
}

/// Returns the `count * width` byte region a pointer list occupies.
///
/// `ptr_pos` and `count` come straight from the header, so the extent is computed with checked
/// arithmetic - otherwise a large value wraps and the bounds check below passes vacuously.
fn pointer_list_view(master_view: &Mmap, ptr_pos: u64, count: u32, width: usize) -> Result<&[u8]> {
    let start = usize::try_from(ptr_pos)?;
    let len = usize::try_from(count)?
        .checked_mul(width)
        .ok_or(Error::OutOfBounds)?;
    let end = start.checked_add(len).ok_or(Error::OutOfBounds)?;

    master_view.get(start..end).ok_or(Error::OutOfBounds)
}

/// Parses the Path Pointer List (called the URL Pointer List before April 2024).
/// See https://wiki.openzim.org/wiki/ZIM_file_format#Path_Pointer_List_(pathPtrPos)
fn parse_url_list(master_view: &Mmap, ptr_pos: u64, count: u32) -> Result<Vec<u64>> {
    let mut cur = Cursor::new(pointer_list_view(master_view, ptr_pos, count, 8)?);

    let mut out: Vec<u64> = Vec::new();
    for _ in 0..count {
        out.push(cur.read_u64::<LittleEndian>()?);
    }

    Ok(out)
}

fn parse_article_list(master_view: &Mmap, ptr_pos: u64, count: u32) -> Result<Vec<u32>> {
    let mut cur = Cursor::new(pointer_list_view(master_view, ptr_pos, count, 4)?);

    let mut out: Vec<u32> = Vec::new();
    for _ in 0..count {
        out.push(cur.read_u32::<LittleEndian>()?);
    }

    Ok(out)
}

fn parse_cluster_list(master_view: &Mmap, ptr_pos: u64, count: u32) -> Result<Vec<u64>> {
    let mut cluster_cur = Cursor::new(pointer_list_view(master_view, ptr_pos, count, 8)?);

    let mut out: Vec<u64> = Vec::new();
    for _ in 0..count {
        out.push(cluster_cur.read_u64::<LittleEndian>()?);
    }
    Ok(out)
}

/// Read out the the 16 byte long MD5 checksum.
fn read_checksum(master_view: &Mmap, checksum_pos: u64) -> Result<Checksum> {
    let checksum_pos = usize::try_from(checksum_pos)?;
    let end = checksum_pos.checked_add(16).ok_or(Error::OutOfBounds)?;
    match master_view.get(checksum_pos..end) {
        Some(raw) => {
            let mut arr = Array::default();
            arr.copy_from_slice(raw);

            Ok(arr)
        }
        None => Err(Error::MissingChecksum),
    }
}

/// Compute the MD5 checksum of the file.
fn compute_checksum(path: &Path, checksum_pos: u64) -> Result<Checksum> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file.take(checksum_pos));
    let mut buffer = vec![0u8; 1024];
    let mut hasher = Md5::new();

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }

        hasher.update(&buffer[..read]);
    }

    Ok(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use crate::cluster::Compression;

    use super::*;

    #[ignore]
    #[test]
    fn test_zim_ab_all_2017_03() {
        let zim =
            Zim::new("fixtures/wikipedia_ab_all_2017-03.zim").expect("failed to parse fixture");

        assert_eq!(zim.header.version_major, 5);
        let cl0 = zim.get_cluster(0).unwrap();
        assert_eq!(&cl0.get_blob(0).unwrap()[..], &[97, 98, 107][..]);

        let cl1 = zim.get_cluster(zim.header.cluster_count - 1).unwrap();
        let b = cl1.get_blob(0).unwrap();
        assert_eq!(&b[0..10], &[71, 73, 70, 56, 57, 97, 44, 1, 150, 0]);
        assert_eq!(
            &b[b.len() - 10..],
            &[222, 192, 21, 240, 155, 91, 65, 0, 0, 59]
        );

        let entries = zim.iterate_by_urls().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(entries.len(), 3111);
    }

    #[ignore]
    #[test]
    fn test_zim_ab_all_maxi_2022_05() {
        let zim = Zim::new("fixtures/wikipedia_ab_all_maxi_2022-05.zim")
            .expect("failed to parse fixture");

        assert_eq!(zim.header.version_major, 5);
        assert_eq!(zim.header.article_count, 9890);

        let cl0 = zim.get_cluster(0).unwrap();
        assert_eq!(
            &cl0.get_blob(0).unwrap()[..],
            &[50, 48, 50, 50, 45, 48, 53, 45, 49, 52][..]
        );
        assert_eq!(cl0.compression(), Compression::Zstd);

        let cl1 = zim.get_cluster(zim.header.cluster_count - 1).unwrap();
        let b = cl1.get_blob(0).unwrap();
        assert_eq!(&b[0..10], &[15, 13, 88, 97, 112, 105, 97, 110, 32, 71]);
        assert_eq!(
            &b[b.len() - 10..],
            &[148, 79, 82, 254, 154, 242, 15, 122, 255, 0],
        );

        let entries = zim.iterate_by_urls().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(entries.len(), 9890);
    }

    #[test]
    fn test_zim_speedtest_en_mini_2025() {
        let zim = Zim::new("fixtures/speedtest_en_blob-mini_2024-05.zim")
            .expect("failed to parse fixture");

        assert_eq!(zim.header.version_major, 6);
        assert_eq!(zim.header.article_count, 19);

        let cl0 = zim.get_cluster(0).unwrap();
        assert_eq!(
            &cl0.get_blob(0).unwrap()[..],
            &[
                115, 112, 101, 101, 100, 116, 101, 115, 116, 95, 101, 110, 95, 98, 108, 111, 98,
                45, 109, 105, 110, 105
            ][..]
        );
        assert_eq!(cl0.compression(), Compression::Zstd);

        let cl1 = zim.get_cluster(zim.header.cluster_count - 1).unwrap();
        let b = cl1.get_blob(0).unwrap();
        assert_eq!(&b[0..10], &[137, 80, 78, 71, 13, 10, 26, 10, 0, 0]);
        assert_eq!(
            &b[b.len() - 10..],
            &[0, 0, 73, 69, 78, 68, 174, 66, 96, 130],
        );

        let entries = zim.iterate_by_urls().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(entries.len(), 19);
    }

    /// `find_by_path` is a binary search, which is only sound because the format guarantees that
    /// directory entries are stored ordered by namespace byte then path.
    #[test]
    fn entries_are_ordered_by_namespace_and_path() {
        for fixture in [
            "fixtures/speedtest_en_blob-mini_2024-05.zim",
            "fixtures/wikipedia_en_100_2026-04.zim",
        ] {
            let zim = Zim::new(fixture).expect("failed to parse fixture");
            let entries = zim
                .iterate_by_urls()
                .collect::<Result<Vec<_>>>()
                .expect("failed to read entries");

            for pair in entries.windows(2) {
                let before = (pair[0].namespace.as_byte(), pair[0].url.as_str());
                let after = (pair[1].namespace.as_byte(), pair[1].url.as_str());
                assert!(
                    before < after,
                    "{fixture}: {before:?} must precede {after:?}"
                );
            }
        }
    }

    #[test]
    fn find_by_path_locates_well_known_entries() {
        let zim =
            Zim::new("fixtures/wikipedia_en_100_2026-04.zim").expect("failed to parse fixture");

        // The entries a reader needs in order to resolve anything in a 6.1+ archive.
        for (namespace, path) in [
            (Namespace::CategoriesArticle, "mainPage"),
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

    /// Every entry must be findable at exactly its own index, which catches comparator mistakes
    /// that spot checks on a handful of paths would miss.
    #[test]
    fn find_by_path_agrees_with_iteration() {
        let zim = Zim::new("fixtures/speedtest_en_blob-mini_2024-05.zim")
            .expect("failed to parse fixture");

        for (idx, entry) in zim.iterate_by_urls().enumerate() {
            let entry = entry.expect("failed to read entry");
            assert_eq!(
                zim.find_by_path(entry.namespace, &entry.url).unwrap(),
                Some(idx as u32),
                "{:?}/{}",
                entry.namespace,
                entry.url
            );
        }
    }

    /// Contradictory header offsets must be caught when the archive is opened. Otherwise they
    /// surface much later as an out-of-bounds read against an unrelated structure, or - for the
    /// last cluster, whose extent is derived from `checksumPos` - as silently wrong data.
    #[test]
    fn corrupt_headers_are_rejected_on_open() {
        let original = std::fs::read("fixtures/speedtest_en_blob-mini_2024-05.zim")
            .expect("failed to read fixture");
        let file_len = original.len() as u64;

        let cases: [(usize, Vec<u8>, &str); 5] = [
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
        ];

        for (offset, value, what) in cases {
            let mut corrupt = original.clone();
            corrupt[offset..offset + value.len()].copy_from_slice(&value);

            let path = std::env::temp_dir().join(format!("zim-corrupt-header-{offset}.zim"));
            std::fs::write(&path, &corrupt).expect("failed to write temp archive");

            let result = Zim::new(&path);
            std::fs::remove_file(&path).ok();

            match result {
                Err(Error::InvalidHeader(_)) => {}
                Err(other) => panic!("{what}: expected InvalidHeader, got {other:?}"),
                Ok(_) => panic!("{what}: expected the archive to be rejected"),
            }
        }
    }

    /// Indices reaching the public accessors come from the archive itself - redirect targets,
    /// dirent cluster numbers, the header's main page - so a corrupt file must produce an error
    /// rather than abort the calling process.
    #[test]
    fn out_of_range_accessors_error_instead_of_panicking() {
        let zim = Zim::new("fixtures/speedtest_en_blob-mini_2024-05.zim")
            .expect("failed to parse fixture");

        assert!(zim.get_by_url_index(zim.header.article_count).is_err());
        assert!(zim.get_by_url_index(u32::MAX).is_err());

        assert!(zim.get_cluster(zim.header.cluster_count).is_err());
        assert!(zim.get_cluster(u32::MAX).is_err());

        let cluster = zim.get_cluster(0).unwrap();
        assert!(cluster.get_blob(u32::MAX).is_err());

        // Probe upwards for the first rejected blob index. The offset table carries one entry
        // more than there are blobs, and treating that end sentinel as a blob used to hand back
        // a bogus slice instead of erroring.
        let mut blobs = 0u32;
        while cluster.get_blob(blobs).is_ok() {
            blobs += 1;
            assert!(blobs < 10_000, "blob index probe failed to terminate");
        }
        assert!(blobs > 0, "cluster 0 should contain at least one blob");
    }

    #[test]
    fn test_zim_wikipedia_en_100_2026_04() {
        let zim =
            Zim::new("fixtures/wikipedia_en_100_2026-04.zim").expect("failed to parse fixture");

        assert_eq!(zim.header.version_major, 6);
        assert_eq!(zim.header.article_count, 9061);

        let cl0 = zim.get_cluster(0).unwrap();
        assert_eq!(
            &cl0.get_blob(0).unwrap()[..],
            &[87, 105, 107, 105, 112, 101, 100, 105, 97][..]
        );
        assert_eq!(cl0.compression(), Compression::Zstd);

        let cl1 = zim.get_cluster(zim.header.cluster_count - 1).unwrap();
        let b = cl1.get_blob(0).unwrap();
        assert_eq!(&b[0..10], &[15, 13, 88, 97, 112, 105, 97, 110, 32, 71]);
        assert_eq!(
            &b[b.len() - 10..],
            &[8, 121, 111, 100, 101, 108, 0, 18, 160, 4],
        );

        let entries = zim.iterate_by_urls().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(entries.len(), 9061);
    }
}
