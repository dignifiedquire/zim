use std::cmp::Ordering;
use std::io::BufRead;
use std::io::Cursor;
use std::ops::Range;
use std::path::{Path, PathBuf};

use byteorder::{LittleEndian, ReadBytesExt};
use md5::{digest::array::Array, digest::OutputSizeUser, Digest, Md5};

use crate::cluster::Cluster;
use crate::directory_entry::DirectoryEntry;
use crate::directory_iterator::DirectoryIterator;
use crate::errors::{Error, Result};
use crate::mime_type::MimeType;
use crate::namespace::Namespace;
use crate::store::Store;
use crate::target::Target;
use crate::uuid::Uuid;

/// Magic number to recognise the file format, must be 72173914
pub const ZIM_MAGIC_NUMBER: u32 = 72173914;

/// Size of the header, up to and including `checksumPos`.
///
/// The MIME type list directly follows the header, so `mimeListPos` also defines the header's
/// size and is never smaller than this.
const HEADER_SIZE: u64 = 80;

/// Path of the well known main page entry, in the `W` namespace.
const MAIN_PAGE: &str = "mainPage";

/// Every entry ordered by title. Removed from the format in version 6.3.
const LISTING_TITLE_ORDERED_V0: &str = "listing/titleOrdered/v0";

/// Article entries ordered by title. Added in version 6.1.
const LISTING_TITLE_ORDERED_V1: &str = "listing/titleOrdered/v1";

const XAPIAN_FULLTEXT: &str = "fulltext/xapian";
const XAPIAN_TITLE: &str = "title/xapian";

/// A redirect may point at another redirect, but a chain this long is a loop.
const MAX_REDIRECT_HOPS: usize = 50;

/// How much of the archive a single directory entry may occupy.
///
/// An entry's length is only known once its path and title have been parsed, so reading one means
/// taking a window and parsing within it. Paths and titles are far smaller than this in practice.
const MAX_DIRENT_SIZE: u64 = 64 * 1024;

/// How far the MIME type list may extend past `mimeListPos`.
const MAX_MIME_LIST_SIZE: u64 = 64 * 1024;

/// Represents a ZIM file
#[allow(dead_code)]
pub struct Zim {
    // Zim structure data:
    pub header: ZimHeader,

    /// The archive's bytes, spanning its chunk files if it is a split archive.
    pub store: Store,
    /// The path the archive was opened from.
    pub file_path: PathBuf,

    /// List of mimetypes used in this ZIM archive
    pub mime_table: Vec<String>, // a list of mimetypes

    /// MD5 checksum.
    pub checksum: Checksum,
}

pub type Checksum = Array<u8, <Md5 as OutputSizeUser>::OutputSize>;

/// A handle to an entry's content.
///
/// Nothing is read until the handle is used, and the bytes are then borrowed rather than copied.
/// This matters because blobs are not small: a search index runs to gigabytes on a full archive,
/// and the format stores indexes and listings uncompressed, so they are served directly from the
/// mapping. Decompressed clusters are decompressed once and cached, and every blob in the cluster
/// then borrows from that one buffer.
///
/// Use [`Content::with`] or [`Content::write_to`] to avoid copying; [`Content::to_vec`] opts into
/// an allocation explicitly.
pub struct Content<'a> {
    cluster: Cluster<'a>,
    blob: u32,
}

impl<'a> Content<'a> {
    /// Calls `f` with the content's bytes, without copying them.
    pub fn with<T>(&self, f: impl FnOnce(&[u8]) -> T) -> Result<T> {
        let guard = self.cluster.read()?;

        Ok(f(guard.blob(self.blob)?))
    }

    /// Size of the content in bytes.
    pub fn len(&self) -> Result<usize> {
        self.with(<[u8]>::len)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Writes the content out without copying it first.
    pub fn write_to(&self, out: &mut impl std::io::Write) -> Result<()> {
        self.with(|bytes| out.write_all(bytes))??;

        Ok(())
    }

    /// Copies the content into a new `Vec`.
    ///
    /// Prefer [`Content::with`] or [`Content::write_to`] where the copy can be avoided.
    pub fn to_vec(&self) -> Result<Vec<u8>> {
        self.with(<[u8]>::to_vec)
    }
}

/// A list of entry indices, ordered by title.
///
/// Listings are one `u32` per entry, so a full archive's listing is tens of megabytes. The indices
/// are decoded on demand rather than up front.
pub struct Listing<'a> {
    source: Source<'a>,
}

enum Source<'a> {
    /// An `X/listing/...` entry.
    Entry(Content<'a>),
    /// A pointer list addressed directly, as the header's title pointer list is.
    Region {
        store: &'a Store,
        pos: u64,
        count: u32,
    },
}

/// Indices read per `Store` access when walking a pointer list, bounding what a listing that
/// straddles a chunk boundary has to copy.
const LISTING_BATCH: u32 = 16 * 1024;

fn index_at(raw: &[u8], pos: usize) -> Option<u32> {
    raw.get(pos * 4..pos * 4 + 4)
        .map(|raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

impl<'a> Listing<'a> {
    /// Number of indices in the listing.
    pub fn len(&self) -> Result<usize> {
        match &self.source {
            Source::Entry(content) => Ok(content.len()? / 4),
            Source::Region { count, .. } => Ok(*count as usize),
        }
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// The entry index at `pos`.
    pub fn get(&self, pos: usize) -> Result<Option<u32>> {
        match &self.source {
            Source::Entry(content) => content.with(|raw| index_at(raw, pos)),
            Source::Region {
                store,
                pos: base,
                count,
            } => {
                if pos >= *count as usize {
                    return Ok(None);
                }

                let at = base.checked_add(pos as u64 * 4).ok_or(Error::OutOfBounds)?;

                Ok(index_at(&store.slice(at, 4)?, 0))
            }
        }
    }

    /// Calls `f` with each entry index, in order.
    pub fn for_each(&self, mut f: impl FnMut(u32)) -> Result<()> {
        let decode = |raw: &[u8], f: &mut dyn FnMut(u32)| {
            for raw in raw.chunks_exact(4) {
                f(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]));
            }
        };

        match &self.source {
            Source::Entry(content) => content.with(|raw| decode(raw, &mut f)),
            Source::Region { store, pos, count } => {
                let mut done = 0u32;
                while done < *count {
                    let batch = LISTING_BATCH.min(*count - done);
                    let at = pos
                        .checked_add(u64::from(done) * 4)
                        .ok_or(Error::OutOfBounds)?;

                    decode(&store.slice(at, u64::from(batch) * 4)?, &mut f);
                    done += batch;
                }

                Ok(())
            }
        }
    }

    /// Copies the listing into a `Vec`.
    ///
    /// Prefer [`Listing::for_each`] on large archives.
    pub fn to_vec(&self) -> Result<Vec<u32>> {
        let mut out = Vec::with_capacity(self.len()?);
        self.for_each(|idx| out.push(idx))?;

        Ok(out)
    }
}

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
    ///
    /// Obsolete: readers should prefer the `X/listing/titleOrdered/v0` entry and fall back to
    /// this. Format version 6.3 removed both, leaving only the article listing - see
    /// [`Zim::entry_list_by_title`] and [`Zim::article_list_by_title`].
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

        // The pointer lists are read on demand, so check here that they fit - otherwise a bad
        // extent is only discovered on whichever access happens to run off the end.
        for (pos, count, width, what) in [
            (
                self.url_ptr_pos,
                self.article_count,
                8,
                "path pointer list runs past the end of file",
            ),
            (
                self.cluster_ptr_pos,
                self.cluster_count,
                8,
                "cluster pointer list runs past the end of file",
            ),
            (
                self.title_ptr_pos.unwrap_or(0),
                self.title_ptr_pos.map_or(0, |_| self.article_count),
                4,
                "title pointer list runs past the end of file",
            ),
        ] {
            let end = u64::from(count)
                .checked_mul(width)
                .and_then(|len| pos.checked_add(len))
                .ok_or(Error::InvalidHeader(what))?;

            if end > file_len {
                return Err(Error::InvalidHeader(what));
            }
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
        let store = Store::open(p.as_ref())?;

        let (header, mime_table) = parse_header(&store)?;
        let checksum = read_checksum(&store, header.checksum_pos)?;

        Ok(Zim {
            header,
            file_path: p.as_ref().into(),
            store,
            mime_table,
            checksum,
        })
    }

    /// Offset of the directory entry at `idx` in the path pointer list.
    fn url_offset(&self, idx: u32) -> Result<u64> {
        self.pointer(self.header.url_ptr_pos, idx, self.header.article_count)
    }

    /// Offset of cluster `idx` in the cluster pointer list.
    fn cluster_offset(&self, idx: u32) -> Result<u64> {
        self.pointer(self.header.cluster_ptr_pos, idx, self.header.cluster_count)
    }

    /// Reads one 8 byte pointer out of a pointer list.
    ///
    /// The lists are read from the mapping on demand rather than materialised at open. A full
    /// archive has millions of entries, and at 8 bytes each the path pointer list alone would
    /// outweigh everything else the reader holds.
    fn pointer(&self, base: u64, idx: u32, count: u32) -> Result<u64> {
        if idx >= count {
            return Err(Error::OutOfBounds);
        }

        let at = base
            .checked_add(u64::from(idx) * 8)
            .ok_or(Error::OutOfBounds)?;

        let mut raw = [0u8; 8];
        raw.copy_from_slice(&self.store.slice(at, 8)?);

        Ok(u64::from_le_bytes(raw))
    }

    /// Computes the checksum, and returns an error if it does not match the one in
    /// the file.
    pub fn verify_checksum(&self) -> Result<()> {
        let checksum_computed = compute_checksum(&self.store, self.header.checksum_pos);

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
                // The caller turns `None` into `Error::UnknownMimeType`; a library must not write
                // to the consumer's stdout.
                self.mime_table
                    .get(id as usize)
                    .map(|mime| MimeType::Type(mime.clone()))
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
        let entry_offset = self.url_offset(idx)?;
        let dir_view = self.store.slice_upto(entry_offset, MAX_DIRENT_SIZE)?;

        DirectoryEntry::new(self, &dir_view)
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
        let idx = self.lower_bound(namespace.as_byte(), path)?;

        if idx < self.header.article_count {
            let entry = self.get_by_url_index(idx)?;
            if entry.namespace == namespace && entry.url == path {
                return Ok(Some(idx));
            }
        }

        Ok(None)
    }

    /// Index of the first entry that is not ordered before `namespace`/`path`.
    fn lower_bound(&self, namespace: u8, path: &str) -> Result<u32> {
        let target = (namespace, path.as_bytes());

        let mut low = 0usize;
        let mut high = self.header.article_count as usize;

        while low < high {
            let mid = low + (high - low) / 2;
            let entry = self.get_by_url_index(mid as u32)?;

            match (entry.namespace.as_byte(), entry.url.as_bytes()).cmp(&target) {
                Ordering::Less => low = mid + 1,
                _ => high = mid,
            }
        }

        Ok(low as u32)
    }

    /// Returns the entry at `namespace`/`path`, if it exists.
    pub fn get_by_path(&self, namespace: Namespace, path: &str) -> Result<Option<DirectoryEntry>> {
        match self.find_by_path(namespace, path)? {
            Some(idx) => Ok(Some(self.get_by_url_index(idx)?)),
            None => Ok(None),
        }
    }

    /// The range of entry indices belonging to `namespace`.
    ///
    /// Both ends are located by binary search, so the entries in between are never touched.
    pub fn namespace_range(&self, namespace: Namespace) -> Result<Range<u32>> {
        let start = self.lower_bound(namespace.as_byte(), "")?;
        let end = match namespace.as_byte().checked_add(1) {
            Some(next) => self.lower_bound(next, "")?,
            None => self.header.article_count,
        };

        Ok(start..end)
    }

    /// Returns a handle to an entry's content, or `None` if it stores none (a redirect, or one
    /// of the deprecated linktarget/deleted entries).
    ///
    /// The bytes are not touched until the handle is used, and are then borrowed rather than
    /// copied - see [`Content`].
    pub fn entry_content(&self, entry: &DirectoryEntry) -> Result<Option<Content<'_>>> {
        match entry.target {
            Some(Target::Cluster(cluster_idx, blob_idx)) => Ok(Some(Content {
                cluster: self.get_cluster(cluster_idx)?,
                blob: blob_idx,
            })),
            _ => Ok(None),
        }
    }

    /// Follows redirects until reaching an entry that stores content.
    ///
    /// A redirect may legally point at another redirect, so the chain is followed rather than
    /// resolved in a single hop.
    pub fn resolve(&self, entry: DirectoryEntry) -> Result<DirectoryEntry> {
        let mut entry = entry;

        for _ in 0..MAX_REDIRECT_HOPS {
            match entry.target {
                Some(Target::Redirect(idx)) => entry = self.get_by_url_index(idx)?,
                _ => return Ok(entry),
            }
        }

        Err(Error::RedirectLoop)
    }

    /// Returns the archive's main page, with the redirect followed.
    ///
    /// Prefers the well known `W/mainPage` entry, falling back to the header's index - well known
    /// entries are optional, so a reader must cope with it being absent.
    pub fn main_page(&self) -> Result<Option<DirectoryEntry>> {
        let entry = match self.get_by_path(Namespace::CategoriesArticle, MAIN_PAGE)? {
            Some(entry) => Some(entry),
            None => match self.header.main_page {
                Some(idx) => Some(self.get_by_url_index(idx)?),
                None => None,
            },
        };

        match entry {
            Some(entry) => Ok(Some(self.resolve(entry)?)),
            None => Ok(None),
        }
    }

    /// Returns the named metadata entry, e.g. `Title` or `Language`.
    pub fn metadata(&self, name: &str) -> Result<Option<Content<'_>>> {
        match self.get_by_path(Namespace::Metadata, name)? {
            Some(entry) => self.entry_content(&entry),
            None => Ok(None),
        }
    }

    /// Returns the names of every metadata entry present.
    ///
    /// The metadata namespace holds on the order of a dozen entries, so unlike the content
    /// namespace it is reasonable to collect.
    pub fn metadata_keys(&self) -> Result<Vec<String>> {
        self.namespace_range(Namespace::Metadata)?
            .map(|idx| Ok(self.get_by_url_index(idx)?.url))
            .collect()
    }

    /// Returns every entry ordered by title.
    ///
    /// Prefers the `X/listing/titleOrdered/v0` entry and falls back to the header's title pointer
    /// list, as the spec directs. Format version 6.3 removed both, so this returns `None` there -
    /// use [`Zim::article_list_by_title`] instead.
    pub fn entry_list_by_title(&self) -> Result<Option<Listing<'_>>> {
        if let Some(listing) = self.listing(LISTING_TITLE_ORDERED_V0)? {
            return Ok(Some(listing));
        }

        Ok(self.header.title_ptr_pos.map(|pos| Listing {
            source: Source::Region {
                store: &self.store,
                pos,
                count: self.header.article_count,
            },
        }))
    }

    /// Returns the archive's article entries ordered by title.
    ///
    /// This is the `X/listing/titleOrdered/v1` entry added in format version 6.1. Unlike
    /// [`Zim::entry_list_by_title`] it covers only article entries, so it is what you want in
    /// order to list or sample articles without resources mixed in.
    pub fn article_list_by_title(&self) -> Result<Option<Listing<'_>>> {
        self.listing(LISTING_TITLE_ORDERED_V1)
    }

    /// Returns the Xapian fulltext index, to be opened with a Xapian implementation.
    ///
    /// This index is the largest thing in a typical archive - gigabytes on a full Wikipedia - so
    /// the handle borrows it rather than reading it into memory.
    pub fn fulltext_index(&self) -> Result<Option<Content<'_>>> {
        self.index_entry(XAPIAN_FULLTEXT)
    }

    /// Returns the Xapian title index, to be opened with a Xapian implementation.
    pub fn title_index(&self) -> Result<Option<Content<'_>>> {
        self.index_entry(XAPIAN_TITLE)
    }

    fn listing(&self, path: &str) -> Result<Option<Listing<'_>>> {
        let Some(content) = self.index_entry(path)? else {
            return Ok(None);
        };

        if content.len()? % 4 != 0 {
            return Err(Error::InvalidListing);
        }

        Ok(Some(Listing {
            source: Source::Entry(content),
        }))
    }

    fn index_entry(&self, path: &str) -> Result<Option<Content<'_>>> {
        match self.get_by_path(Namespace::FulltextIndex, path)? {
            Some(entry) => self.entry_content(&entry),
            None => Ok(None),
        }
    }

    /// Returns the given `Cluster`
    ///
    /// idx must be between 0 and `cluster_count`
    pub fn get_cluster(&self, idx: u32) -> Result<Cluster<'_>> {
        let start = self.cluster_offset(idx)?;
        // The last cluster runs up to the checksum, which is always the final 16 bytes.
        let end = if idx + 1 < self.header.cluster_count {
            self.cluster_offset(idx + 1)?
        } else {
            self.header.checksum_pos
        };

        Cluster::new(&self.store, start, end, self.header.version_major)
    }
}

fn is_defined(val: u32) -> Option<u32> {
    if val == 0xffffffff {
        None
    } else {
        Some(val)
    }
}

fn parse_header(store: &Store) -> Result<(ZimHeader, Vec<String>)> {
    let file_len = store.len();
    if file_len < HEADER_SIZE {
        return Err(Error::InvalidHeader("file is smaller than the header"));
    }

    let fixed = store.slice(0, HEADER_SIZE)?;
    let mut header_cur = Cursor::new(&fixed[..]);

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

    // The MIME type list directly follows the header, so read from `mime_list_pos` rather than
    // assuming the length of the fields read above. A header extended by a future minor version
    // is then skipped instead of being parsed as MIME types.
    let mime_region = store.slice_upto(mime_list_pos, MAX_MIME_LIST_SIZE)?;
    let mut header_cur = Cursor::new(&mime_region[..]);

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

/// Read out the the 16 byte long MD5 checksum.
fn read_checksum(store: &Store, checksum_pos: u64) -> Result<Checksum> {
    let raw = store
        .slice(checksum_pos, 16)
        .map_err(|_| Error::MissingChecksum)?;

    let mut arr = Array::default();
    arr.copy_from_slice(&raw);

    Ok(arr)
}

/// Compute the MD5 checksum over everything preceding the stored checksum.
///
/// For a split archive this spans the chunk files, since the checksum covers the archive as a
/// whole rather than any one chunk.
fn compute_checksum(store: &Store, checksum_pos: u64) -> Checksum {
    let mut hasher = Md5::new();

    for chunk in store.prefix_chunks(checksum_pos) {
        hasher.update(chunk);
    }

    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use testdir::testdir;

    use super::*;

    /// A pointer list is walked in bounded batches so that a listing straddling a chunk boundary
    /// does not have to be copied whole. The batch seams must not drop or duplicate indices.
    ///
    /// This reaches into `Listing`'s internals, so it lives here rather than with the
    /// fixture-driven tests: the region source is only reachable through the archive as a
    /// fallback, which none of the test archives trigger.
    #[test]
    fn region_listing_walks_batch_boundaries_correctly() {
        let count = LISTING_BATCH * 2 + 7;
        let expected: Vec<u32> = (0..count).map(|i| i.wrapping_mul(2_654_435_761)).collect();

        let mut raw = Vec::new();
        for value in &expected {
            raw.extend_from_slice(&value.to_le_bytes());
        }

        let dir: PathBuf = testdir!();
        let path = dir.join("pointers.bin");
        std::fs::write(&path, &raw).expect("failed to write pointer list");

        let store = Store::open(&path).expect("failed to map pointer list");
        let listing = Listing {
            source: Source::Region {
                store: &store,
                pos: 0,
                count,
            },
        };

        assert_eq!(listing.len().unwrap(), count as usize);
        assert_eq!(listing.to_vec().unwrap(), expected);

        // Straddling a seam, and just past the end.
        assert_eq!(
            listing.get(LISTING_BATCH as usize).unwrap(),
            Some(expected[LISTING_BATCH as usize])
        );
        assert_eq!(listing.get(count as usize).unwrap(), None);
    }
}
