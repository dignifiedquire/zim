use std::fmt;
use std::io::Cursor;
use std::io::Read;
use std::ops::Deref;
use std::sync::{Arc, RwLock};

use bitreader::BitReader;
use byteorder::{LittleEndian, ReadBytesExt};
use memmap::Mmap;
use ouroboros::self_referencing;
use xz2::read::XzDecoder;

use crate::errors::{Error, Result};

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
    extended: bool,
    compression: Compression,
    start: u64,
    end: u64,
    size: u64,
    view: &'a [u8],
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
            .field("size", &raw.size)
            .field("view len", &raw.view.len())
            .field("blob_list", &raw.blob_list)
            .field(
                "decompressed len",
                &raw.decompressed.as_ref().map(|s| s.len()),
            )
            .finish()
    }
}

impl<'a> Cluster<'a> {
    pub fn new(
        master_view: &'a Mmap,
        cluster_list: &'a [u64],
        idx: u32,
        checksum_pos: u64,
        version: u16,
    ) -> Result<Cluster<'a>> {
        Ok(Cluster(Arc::new(RwLock::new(InnerCluster::new(
            master_view,
            cluster_list,
            idx,
            checksum_pos,
            version,
        )?))))
    }

    pub fn decompress(&self) -> Result<()> {
        self.0.write().unwrap().decompress()
    }

    pub fn compression(&self) -> Compression {
        self.0.read().unwrap().compression
    }

    /// Returns a copy of the blob's bytes.
    ///
    /// Unlike [`Cluster::get_blob`], the result does not borrow from the cluster, so this can be
    /// used with a `Cluster` that is dropped straight away.
    pub fn blob_to_vec(&self, idx: u32) -> Result<Vec<u8>> {
        {
            let lock = self.0.read().unwrap();
            if lock.needs_decompression() {
                drop(lock);
                self.0.write().unwrap().decompress()?;
            }
        }

        let guard = self.0.read().unwrap();
        Ok(guard.get_blob(idx)?.to_vec())
    }

    pub fn get_blob<'b: 'a>(&'b self, idx: u32) -> Result<Blob<'a, 'b>> {
        {
            let lock = self.0.read().unwrap();
            if lock.needs_decompression() {
                drop(lock);
                self.0.write().unwrap().decompress()?;
            }
        }

        let blob = BlobTryBuilder {
            guard: self.0.read().unwrap(),
            slice_builder: |guard| guard.get_blob(idx),
        }
        .try_build()?;

        Ok(blob)
    }
}

#[self_referencing]
pub struct Blob<'a, 'b: 'a> {
    guard: std::sync::RwLockReadGuard<'b, InnerCluster<'a>>,
    #[borrows(guard)]
    slice: &'this [u8],
}

impl<'a, 'b: 'a> Deref for Blob<'a, 'b> {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        self.borrow_slice()
    }
}

impl<'a, 'b: 'a> AsRef<[u8]> for Blob<'a, 'b> {
    fn as_ref(&self) -> &[u8] {
        self.borrow_slice()
    }
}

impl<'a> InnerCluster<'a> {
    fn new(
        master_view: &'a Mmap,
        cluster_list: &'a [u64],
        idx: u32,
        checksum_pos: u64,
        version: u16,
    ) -> Result<Self> {
        let idx = idx as usize;
        let start = *cluster_list.get(idx).ok_or(Error::OutOfBounds)?;
        // The last cluster runs up to the checksum, which is always the final 16 bytes.
        let end = match cluster_list.get(idx + 1) {
            Some(next) => *next,
            None => checksum_pos,
        };

        if end <= start {
            return Err(Error::OutOfBounds);
        }
        let cluster_size = end - start;
        let cluster_view = master_view
            .get(usize::try_from(start)?..usize::try_from(end)?)
            .ok_or(Error::OutOfBounds)?;

        let (extended, compression) =
            parse_details(cluster_view.first().ok_or(Error::OutOfBounds)?)?;

        // extended clusters are only allowed in version 6
        if extended && version != 6 {
            return Err(Error::InvalidClusterExtension);
        }

        let blob_list = if Compression::None == compression {
            let cur = Cursor::new(&cluster_view[1..]);
            Some(parse_blob_list(cur, extended)?)
        } else {
            None
        };

        Ok(Self {
            extended,
            compression,
            start,
            end,
            size: cluster_size,
            view: cluster_view,
            decompressed: None,
            blob_list,
        })
    }

    fn needs_decompression(&self) -> bool {
        match self.compression {
            Compression::Lzma2 | Compression::Zstd => {
                self.decompressed.is_none() || self.blob_list.is_none()
            }
            Compression::None => false,
        }
    }

    fn decompress(&mut self) -> Result<()> {
        let payload = self.view.get(1..).ok_or(Error::OutOfBounds)?;

        if self.decompressed.is_none() {
            match self.compression {
                Compression::Lzma2 => {
                    let mut decoder = XzDecoder::new(payload);
                    let mut d = Vec::with_capacity(self.view.len());
                    decoder.read_to_end(&mut d)?;
                    self.decompressed = Some(d);
                }
                Compression::Zstd => {
                    let out = zstd::stream::decode_all(payload)?;
                    self.decompressed = Some(out);
                }
                Compression::None => {}
            }
        }

        if self.blob_list.is_none() {
            match self.compression {
                Compression::Lzma2 | Compression::Zstd => {
                    let decompressed = self.decompressed.as_ref().ok_or(Error::MissingBlobList)?;
                    let blob_list = parse_blob_list(Cursor::new(decompressed), self.extended)?;
                    self.blob_list = Some(blob_list);
                }
                Compression::None => {}
            }
        }

        Ok(())
    }

    fn get_blob(&self, idx: u32) -> Result<&[u8]> {
        let list = self.blob_list.as_ref().ok_or(Error::MissingBlobList)?;

        // The offset table always holds one more entry than there are blobs; the final entry is
        // the end of the data area. So `idx` is only valid while `idx + 1` is still in the table.
        let idx = idx as usize;
        let start = usize::try_from(*list.get(idx).ok_or(Error::OutOfBounds)?)?;
        let end = usize::try_from(*list.get(idx + 1).ok_or(Error::OutOfBounds)?)?;

        match self.compression {
            Compression::Lzma2 | Compression::Zstd => self
                .decompressed
                .as_ref()
                .ok_or(Error::MissingBlobList)?
                .get(start..end)
                .ok_or(Error::OutOfBounds),
            // Offsets are relative to the start of the offset table, which follows the info byte.
            Compression::None => self.view.get(1 + start..1 + end).ok_or(Error::OutOfBounds),
        }
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
