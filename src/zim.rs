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
    use crate::cluster::Compression;

    use super::*;

    /// Copies a blob out, for comparing against expected bytes.
    fn blob(zim: &Zim, cluster: u32, idx: u32) -> Vec<u8> {
        let cluster = zim.get_cluster(cluster).unwrap();
        let guard = cluster.read().unwrap();

        guard.blob(idx).unwrap().to_vec()
    }

    #[ignore]
    #[test]
    fn test_zim_ab_all_2017_03() {
        let zim =
            Zim::new("fixtures/wikipedia_ab_all_2017-03.zim").expect("failed to parse fixture");

        assert_eq!(zim.header.version_major, 5);
        assert_eq!(blob(&zim, 0, 0), &[97, 98, 107][..]);

        let b = blob(&zim, zim.header.cluster_count - 1, 0);
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

        assert_eq!(
            blob(&zim, 0, 0),
            &[50, 48, 50, 50, 45, 48, 53, 45, 49, 52][..]
        );
        assert_eq!(zim.get_cluster(0).unwrap().compression(), Compression::Zstd);

        let b = blob(&zim, zim.header.cluster_count - 1, 0);
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

        assert_eq!(
            blob(&zim, 0, 0),
            &[
                115, 112, 101, 101, 100, 116, 101, 115, 116, 95, 101, 110, 95, 98, 108, 111, 98,
                45, 109, 105, 110, 105
            ][..]
        );
        assert_eq!(zim.get_cluster(0).unwrap().compression(), Compression::Zstd);

        let b = blob(&zim, zim.header.cluster_count - 1, 0);
        assert_eq!(&b[0..10], &[137, 80, 78, 71, 13, 10, 26, 10, 0, 0]);
        assert_eq!(
            &b[b.len() - 10..],
            &[0, 0, 73, 69, 78, 68, 174, 66, 96, 130],
        );

        let entries = zim.iterate_by_urls().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(entries.len(), 19);
    }

    /// Everything an archive exposes, for comparing two ways of opening the same bytes.
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

    /// Writes `raw` out as chunk files named `archive.zimaa`, `archive.zimab`, ... in `dir`, and
    /// returns the path of the archive as a whole - which deliberately does not exist.
    fn write_chunks(dir: &std::path::Path, raw: &[u8], chunk_size: usize) -> PathBuf {
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

    /// A split archive is the concatenation of its chunks, so it must read exactly like the
    /// whole file - including for structures that straddle a chunk boundary and therefore have
    /// no contiguous backing to borrow from.
    #[test]
    fn reads_a_split_archive_identically_to_the_whole_file() {
        let source = "fixtures/speedtest_en_blob-mini_2024-05.zim";

        let single = Zim::new(source).expect("failed to parse fixture");
        single
            .verify_checksum()
            .expect("fixture checksum must match");
        let expected = snapshot(&single);

        let raw = std::fs::read(source).expect("failed to read fixture");

        // Boundaries chosen to land in the directory entries, the clusters, the pointer lists,
        // and inside the trailing checksum respectively. The 26x26 naming scheme caps the chunk
        // count at 676, so none of these may be too small.
        for chunk_size in [10_000, 300_000, raw.len() / 2, 2_064_000, raw.len() - 1] {
            let dir = std::env::temp_dir().join(format!("zim-split-{chunk_size}"));
            let _ = std::fs::remove_dir_all(&dir);

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

            std::fs::remove_dir_all(&dir).ok();
        }
    }

    /// A whole archive takes precedence over chunks sharing its name, and a chunk sequence stops
    /// at the first gap rather than skipping over it.
    #[test]
    fn chunk_discovery_prefers_the_whole_archive_and_stops_at_a_gap() {
        let raw = std::fs::read("fixtures/speedtest_en_blob-mini_2024-05.zim")
            .expect("failed to read fixture");

        let dir = std::env::temp_dir().join("zim-split-discovery");
        let _ = std::fs::remove_dir_all(&dir);

        let base = write_chunks(&dir, &raw, raw.len() / 3);

        // Dropping the second chunk truncates the archive, which must fail the header's
        // "checksum sits 16 bytes before the end" invariant rather than read short.
        std::fs::remove_file(dir.join("archive.zimab")).unwrap();
        assert!(Zim::new(&base).is_err(), "a gap must not be skipped over");

        // With the whole archive present under the same name, the chunks are ignored.
        std::fs::write(&base, &raw).unwrap();
        let whole = Zim::new(&base).expect("failed to open whole archive");
        assert_eq!(whole.store.len(), raw.len() as u64);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolves_the_main_page_through_its_redirect() {
        // The header's main page index points at `W/mainPage`, which is a redirect into `C`.
        // Reporting that stub instead of following it yields the useless path "mainPage".
        for (fixture, expected) in [
            ("fixtures/speedtest_en_blob-mini_2024-05.zim", "home.html"),
            ("fixtures/wikipedia_en_100_2026-04.zim", "index"),
        ] {
            let zim = Zim::new(fixture).expect("failed to parse fixture");
            let main = zim
                .main_page()
                .unwrap()
                .expect("fixture should have a main page");

            assert_eq!(main.url, expected, "{fixture}");
            assert_eq!(main.namespace, Namespace::UserContent, "{fixture}");
        }
    }

    #[test]
    fn reads_metadata() {
        let zim =
            Zim::new("fixtures/wikipedia_en_100_2026-04.zim").expect("failed to parse fixture");

        let title = zim
            .metadata("Title")
            .unwrap()
            .expect("Title is mandatory")
            .to_vec()
            .unwrap();
        assert_eq!(String::from_utf8(title).unwrap(), "Wikipedia 100");

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
        let zim =
            Zim::new("fixtures/wikipedia_en_100_2026-04.zim").expect("failed to parse fixture");

        // Sizes are read through the handle rather than by loading the index: on a full archive
        // the fulltext index runs to gigabytes.
        let fulltext = zim.fulltext_index().unwrap().expect("fulltext index");
        assert_eq!(fulltext.len().unwrap(), 2_424_832);

        let title = zim.title_index().unwrap().expect("title index");
        assert_eq!(title.len().unwrap(), 917_504);
    }

    /// The spec says `titlePtrPos` points directly at the `v0` listing entry's data, so the
    /// header fallback must produce exactly what the entry does.
    #[test]
    fn title_pointer_fallback_matches_the_v0_listing() {
        let zim = Zim::new("fixtures/speedtest_en_blob-mini_2024-05.zim")
            .expect("failed to parse fixture");

        let via_entry = zim
            .entry_list_by_title()
            .unwrap()
            .expect("6.2 archives carry the v0 listing")
            .to_vec()
            .unwrap();
        assert_eq!(via_entry.len(), zim.header.article_count as usize);

        let header_listing = Listing {
            source: Source::Region {
                store: &zim.store,
                pos: zim
                    .header
                    .title_ptr_pos
                    .expect("6.2 archives carry titlePtrPos"),
                count: zim.header.article_count,
            },
        };
        assert_eq!(header_listing.to_vec().unwrap(), via_entry);

        // Random access must agree with the sequential walk, and stop at the end.
        for (pos, expected) in via_entry.iter().enumerate() {
            assert_eq!(header_listing.get(pos).unwrap(), Some(*expected));
        }
        assert_eq!(header_listing.get(via_entry.len()).unwrap(), None);
    }

    /// A pointer list is walked in bounded batches so that a listing straddling a chunk boundary
    /// does not have to be copied whole. The batch seams must not drop or duplicate indices.
    #[test]
    fn region_listing_walks_batch_boundaries_correctly() {
        let zim =
            Zim::new("fixtures/wikipedia_en_100_2026-04.zim").expect("failed to parse fixture");

        // Any region of the archive decodes as a list of u32; the values are meaningless here,
        // the point is that the walk spans several batches.
        let count = LISTING_BATCH * 2 + 7;
        let pos = 4096u64;

        let listing = Listing {
            source: Source::Region {
                store: &zim.store,
                pos,
                count,
            },
        };

        let raw = zim.store.slice(pos, u64::from(count) * 4).unwrap();
        let expected: Vec<u32> = raw
            .chunks_exact(4)
            .map(|raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
            .collect();

        assert_eq!(listing.len().unwrap(), count as usize);
        assert_eq!(listing.to_vec().unwrap(), expected);
        assert_eq!(
            listing.get(LISTING_BATCH as usize).unwrap(),
            Some(expected[LISTING_BATCH as usize])
        );
    }

    /// Asking a cluster about itself must not pull its data in. Clusters are not small - the
    /// largest in this fixture is 78MB - so a reader that inspects every cluster, as `zim-info`
    /// does, would otherwise fault in most of the archive.
    #[test]
    fn cluster_data_loads_only_on_blob_access() {
        let zim =
            Zim::new("fixtures/wikipedia_en_100_2026-04.zim").expect("failed to parse fixture");

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

    /// Content from an uncompressed cluster must be borrowed straight from the mapping rather
    /// than copied. This is what keeps a multi-gigabyte search index from having to be read into
    /// memory in order to be used, and the format requires indexes and listings to be stored
    /// uncompressed precisely so that this is possible.
    #[test]
    fn index_content_is_borrowed_from_the_mapping() {
        let zim =
            Zim::new("fixtures/wikipedia_en_100_2026-04.zim").expect("failed to parse fixture");

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

    /// Version 6.3 removed both the header's title pointer list and the `v0` listing, leaving
    /// only the article listing. Title ordering must still be reachable for such archives.
    #[test]
    fn title_listings_track_the_format_version() {
        let v62 = Zim::new("fixtures/speedtest_en_blob-mini_2024-05.zim")
            .expect("failed to parse fixture");
        assert_eq!((v62.header.version_major, v62.header.version_minor), (6, 2));
        assert!(v62.header.title_ptr_pos.is_some());
        assert!(v62.entry_list_by_title().unwrap().is_some());
        assert!(v62.article_list_by_title().unwrap().is_some());

        let v63 =
            Zim::new("fixtures/wikipedia_en_100_2026-04.zim").expect("failed to parse fixture");
        assert_eq!((v63.header.version_major, v63.header.version_minor), (6, 3));
        assert!(v63.header.title_ptr_pos.is_none());
        assert!(v63.entry_list_by_title().unwrap().is_none());

        let articles = v63
            .article_list_by_title()
            .unwrap()
            .expect("6.3 archives carry the v1 listing");
        assert!(!articles.is_empty().unwrap());
        assert!(articles.len().unwrap() < v63.header.article_count as usize);

        // The listing must decode to real indices, in title order - a wrong element width or
        // endianness would still produce a plausible-looking list of numbers.
        let mut titles: Vec<String> = Vec::new();
        articles
            .for_each(|idx| {
                let entry = v63
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
            "v1 listing is not in title order"
        );
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
            // Valid on its own, but the list it points at cannot fit in what is left of the file.
            // The lists are read on demand, so this has to be caught here rather than on access.
            (
                32,
                (file_len - 10).to_le_bytes().into(),
                "path pointer list running past the end",
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
        let guard = cluster.read().unwrap();
        assert!(guard.blob(u32::MAX).is_err());

        // The offset table carries one entry more than there are blobs. Treating that end
        // sentinel as a blob used to hand back a bogus slice instead of erroring.
        let count = guard.blob_count() as u32;
        assert!(count > 0, "cluster 0 should contain at least one blob");
        assert!(guard.blob(count - 1).is_ok(), "last blob must be readable");
        assert!(guard.blob(count).is_err(), "the end sentinel is not a blob");
    }

    #[test]
    fn test_zim_wikipedia_en_100_2026_04() {
        let zim =
            Zim::new("fixtures/wikipedia_en_100_2026-04.zim").expect("failed to parse fixture");

        assert_eq!(zim.header.version_major, 6);
        assert_eq!(zim.header.article_count, 9061);

        assert_eq!(
            blob(&zim, 0, 0),
            &[87, 105, 107, 105, 112, 101, 100, 105, 97][..]
        );
        assert_eq!(zim.get_cluster(0).unwrap().compression(), Compression::Zstd);

        let b = blob(&zim, zim.header.cluster_count - 1, 0);
        assert_eq!(&b[0..10], &[15, 13, 88, 97, 112, 105, 97, 110, 32, 71]);
        assert_eq!(
            &b[b.len() - 10..],
            &[8, 121, 111, 100, 101, 108, 0, 18, 160, 4],
        );

        let entries = zim.iterate_by_urls().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(entries.len(), 9061);
    }
}
