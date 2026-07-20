use std::borrow::Cow;
use std::fmt;
use std::io::Cursor;
use std::io::Read;
use std::sync::{Arc, RwLock, RwLockReadGuard};

use bitreader::BitReader;
use byteorder::{LittleEndian, ReadBytesExt};
use xz2::read::XzDecoder;

use crate::errors::{Error, Result};
use crate::store::Store;

/// The compression applied to a cluster's payload.
///
/// Values 2 (zlib) and 3 (bzip2) were used by earlier writers but have since been removed from
/// the format; libzim cannot read them either, so they are rejected rather than represented here.
/// Value 0 is an obsolete encoding of "no compression" inherited from Zeno.
#[repr(u8)]
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum Compression {
    None = 1,
    Lzma2 = 4,
    Zstd = 5,
}

impl From<Compression> for u8 {
    fn from(mode: Compression) -> u8 {
        mode as u8
    }
}

impl Compression {
    pub fn from(raw: u8) -> Result<Compression> {
        match raw {
            0 | 1 => Ok(Compression::None),
            2 | 3 => Err(Error::UnsupportedCompression(raw)),
            4 => Ok(Compression::Lzma2),
            5 => Ok(Compression::Zstd),
            _ => Err(Error::UnknownCompression(raw)),
        }
    }
}

/// A cluster of blobs
///
/// Within an ZIM archive, clusters contain several blobs of data that are all compressed together.
/// Each blob is the data for an article.
#[derive(Clone)]
pub struct Cluster<'a>(Arc<RwLock<InnerCluster<'a>>>);

pub struct InnerCluster<'a> {
    store: &'a Store,
    extended: bool,
    compression: Compression,
    start: u64,
    end: u64,
    /// The data region, everything after the info byte, for an uncompressed cluster.
    ///
    /// Loaded on first blob access rather than at construction: clusters run to tens of
    /// megabytes, and merely asking a cluster for its compression must not pull it in. Borrowed
    /// from the mapping unless the cluster straddles a chunk boundary, which the format is not
    /// supposed to allow but which costs nothing to support.
    view: Option<Cow<'a, [u8]>>,
    blob_list: Option<Vec<u64>>, // offsets into data
    decompressed: Option<Vec<u8>>,
}

impl<'a> fmt::Debug for Cluster<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let raw = self.0.read().unwrap();
        f.debug_struct("Cluster")
            .field("extended", &raw.extended)
            .field("compression", &raw.compression)
            .field("start", &raw.start)
            .field("end", &raw.end)
            .field("view len", &raw.view.as_ref().map(|view| view.len()))
            .field("blob_list", &raw.blob_list)
            .field(
                "decompressed len",
                &raw.decompressed.as_ref().map(|s| s.len()),
            )
            .finish()
    }
}

impl<'a> Cluster<'a> {
    /// Reads the cluster occupying `start..end`.
    pub fn new(store: &'a Store, start: u64, end: u64, version: u16) -> Result<Cluster<'a>> {
        Ok(Cluster(Arc::new(RwLock::new(InnerCluster::new(
            store, start, end, version,
        )?))))
    }

    /// Loads the cluster's data, decompressing it if necessary.
    pub fn decompress(&self) -> Result<()> {
        self.0.write().unwrap().load()
    }

    pub fn compression(&self) -> Compression {
        self.0.read().unwrap().compression
    }

    /// Whether the cluster's data has been loaded yet.
    #[cfg(test)]
    pub(crate) fn is_loaded(&self) -> bool {
        self.0.read().unwrap().blob_list.is_some()
    }

    /// Locks the cluster for reading, decompressing it first if necessary.
    ///
    /// Blobs are borrowed from the guard rather than copied. A blob can be very large - search
    /// indexes run to gigabytes on a full archive - so the only allocation is the cluster's own
    /// decompression buffer, which is computed once and shared by every blob in the cluster.
    /// Uncompressed clusters, which is where the format requires indexes and listings to live,
    /// are served straight from the mapping and allocate nothing at all.
    pub fn read(&self) -> Result<ClusterGuard<'_, 'a>> {
        {
            let lock = self.0.read().unwrap();
            if lock.needs_loading() {
                drop(lock);
                self.0.write().unwrap().load()?;
            }
        }

        Ok(ClusterGuard(self.0.read().unwrap()))
    }
}

/// Read access to a cluster's blobs.
pub struct ClusterGuard<'g, 'a>(RwLockReadGuard<'g, InnerCluster<'a>>);

impl<'g, 'a> ClusterGuard<'g, 'a> {
    /// Returns a blob's bytes, borrowed from the archive or from the decompression buffer.
    pub fn blob(&self, idx: u32) -> Result<&[u8]> {
        self.0.get_blob(idx)
    }

    /// Number of blobs in the cluster.
    pub fn blob_count(&self) -> usize {
        self.0.blob_count()
    }
}

impl<'a> InnerCluster<'a> {
    fn new(store: &'a Store, start: u64, end: u64, version: u16) -> Result<Self> {
        if end <= start {
            return Err(Error::OutOfBounds);
        }

        // Only the info byte: the data region is loaded on demand.
        let details = store.slice(start, 1)?;
        let (extended, compression) = parse_details(details.first().ok_or(Error::OutOfBounds)?)?;

        // extended clusters are only allowed in version 6
        if extended && version != 6 {
            return Err(Error::InvalidClusterExtension);
        }

        Ok(Self {
            store,
            extended,
            compression,
            start,
            end,
            view: None,
            decompressed: None,
            blob_list: None,
        })
    }

    fn needs_loading(&self) -> bool {
        self.blob_list.is_none()
    }

    /// Makes the cluster's data available, decompressing it if necessary.
    fn load(&mut self) -> Result<()> {
        if self.blob_list.is_some() {
            return Ok(());
        }

        let store = self.store;
        let payload = self.start + 1;

        match self.compression {
            Compression::None => {
                let view = store.slice(payload, self.end - payload)?;
                self.blob_list = Some(parse_blob_list(Cursor::new(&view[..]), self.extended)?);
                self.view = Some(view);
            }
            // The compressed bytes are streamed into the decoder rather than materialised first.
            Compression::Lzma2 => {
                let mut out = Vec::new();
                XzDecoder::new(store.reader(payload, self.end)).read_to_end(&mut out)?;
                self.blob_list = Some(parse_blob_list(Cursor::new(&out[..]), self.extended)?);
                self.decompressed = Some(out);
            }
            Compression::Zstd => {
                let out = zstd::stream::decode_all(store.reader(payload, self.end))?;
                self.blob_list = Some(parse_blob_list(Cursor::new(&out[..]), self.extended)?);
                self.decompressed = Some(out);
            }
        }

        Ok(())
    }

    /// The cluster's data, blob offsets index into this.
    fn data(&self) -> Result<&[u8]> {
        match (self.view.as_ref(), self.decompressed.as_ref()) {
            (Some(view), _) => Ok(view),
            (_, Some(decompressed)) => Ok(decompressed),
            _ => Err(Error::MissingBlobList),
        }
    }

    /// The offset table carries one entry more than there are blobs, the last being the end of
    /// the data area.
    fn blob_count(&self) -> usize {
        self.blob_list
            .as_ref()
            .map_or(0, |list| list.len().saturating_sub(1))
    }

    fn get_blob(&self, idx: u32) -> Result<&[u8]> {
        let list = self.blob_list.as_ref().ok_or(Error::MissingBlobList)?;

        // The offset table always holds one more entry than there are blobs; the final entry is
        // the end of the data area. So `idx` is only valid while `idx + 1` is still in the table.
        let idx = idx as usize;
        let start = usize::try_from(*list.get(idx).ok_or(Error::OutOfBounds)?)?;
        let end = usize::try_from(*list.get(idx + 1).ok_or(Error::OutOfBounds)?)?;

        // Offsets are relative to the start of the offset table, which is the data region.
        self.data()?.get(start..end).ok_or(Error::OutOfBounds)
    }
}

/// Parses the cluster information.
///
/// Fourth low bits:
///   - 0: default (no compression),
///   - 1: none (inherited from Zeno),
///   - 4: LZMA2 compressed
///
/// Firth bits:
///   - 0: normal (OFFSET_SIZE=4)
///   - 1: extended (OFFSET_SIZE=8)
fn parse_details(details: &u8) -> Result<(bool, Compression)> {
    let slice = &[*details];
    let mut reader = BitReader::new(slice);
    // skip first three bits
    reader.skip(3)?;

    // extended mode is the 4th bits from the left
    // compression are the last four bits

    Ok((reader.read_bool()?, Compression::from(reader.read_u8(4)?)?))
}

/// Parses a cluster's blob offset table.
///
/// The table holds one offset per blob plus a final offset marking the end of the data area, so a
/// cluster with N blobs has N+1 offsets. Offsets are relative to the start of the table, which
/// means the first offset is also the table's own size - dividing it by the offset size yields the
/// number of entries.
fn parse_blob_list<T: ReadBytesExt>(mut cur: T, extended: bool) -> Result<Vec<u64>> {
    let offset_size: u64 = if extended { 8 } else { 4 };

    let read_offset = |cur: &mut T| -> Result<u64> {
        Ok(if extended {
            cur.read_u64::<LittleEndian>()?
        } else {
            cur.read_u32::<LittleEndian>()? as u64
        })
    };

    let first = read_offset(&mut cur)?;
    // A table must hold at least the end sentinel, and can only consist of whole offsets.
    if first < offset_size || first % offset_size != 0 {
        return Err(Error::InvalidBlobList);
    }
    let count = first / offset_size;

    // Deliberately not pre-allocating: `count` comes straight off disk.
    let mut blob_list = vec![first];
    let mut prev = first;
    for _ in 1..count {
        let offset = read_offset(&mut cur)?;
        if offset < prev {
            return Err(Error::InvalidBlobList);
        }
        prev = offset;
        blob_list.push(offset);
    }

    Ok(blob_list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_compressions_are_rejected() {
        // zlib (2) and bzip2 (3) were dropped from the format; libzim cannot read them either, so
        // they must fail cleanly rather than reach an unimplemented decoder.
        assert!(matches!(
            Compression::from(2),
            Err(Error::UnsupportedCompression(2))
        ));
        assert!(matches!(
            Compression::from(3),
            Err(Error::UnsupportedCompression(3))
        ));

        // 0 is Zeno's obsolete spelling of "no compression", 1 is the current one.
        assert_eq!(Compression::from(0).unwrap(), Compression::None);
        assert_eq!(Compression::from(1).unwrap(), Compression::None);
        assert_eq!(Compression::from(4).unwrap(), Compression::Lzma2);
        assert_eq!(Compression::from(5).unwrap(), Compression::Zstd);
        assert!(matches!(
            Compression::from(6),
            Err(Error::UnknownCompression(6))
        ));
    }

    #[test]
    fn blob_list_rejects_malformed_first_offset() {
        // The first offset is the table's own size, so it must be a non-zero multiple of the
        // offset width. Anything below the width made the entry count underflow to usize::MAX.
        for bad in [0u32, 1, 2, 3, 6] {
            assert!(
                parse_blob_list(Cursor::new(bad.to_le_bytes()), false).is_err(),
                "first offset {bad} should be rejected"
            );
        }
    }

    #[test]
    fn blob_list_accepts_zero_blob_cluster() {
        // A table holding only the end sentinel is legal and describes a cluster with no blobs.
        let list = parse_blob_list(Cursor::new(4u32.to_le_bytes()), false).unwrap();
        assert_eq!(list, vec![4]);
    }

    #[test]
    fn blob_list_rejects_decreasing_offsets() {
        // Offsets delimit consecutive blobs, so a decreasing pair would yield an inverted range.
        let mut raw = Vec::new();
        raw.extend_from_slice(&12u32.to_le_bytes()); // three offsets
        raw.extend_from_slice(&20u32.to_le_bytes());
        raw.extend_from_slice(&16u32.to_le_bytes()); // goes backwards
        assert!(parse_blob_list(Cursor::new(raw.as_slice()), false).is_err());
    }
}
