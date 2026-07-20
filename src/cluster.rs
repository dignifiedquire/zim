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

#[repr(u8)]
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum Compression {
    None = 0,
    Zlib = 2,
    Bzip2 = 3,
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
            0 => Ok(Compression::None),
            1 => Ok(Compression::None),
            2 => Ok(Compression::Zlib),
            3 => Ok(Compression::Bzip2),
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
        let start = cluster_list[idx];
        let end = if idx < cluster_list.len() - 1 {
            cluster_list[idx + 1]
        } else {
            checksum_pos
        };

        assert!(end > start);
        let cluster_size = end - start;
        let cluster_view = master_view
            .get(start as usize..end as usize)
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
            Compression::Lzma2 | Compression::Bzip2 | Compression::Zlib | Compression::Zstd => {
                self.decompressed.is_none() || self.blob_list.is_none()
            }
            Compression::None => false,
        }
    }

    fn decompress(&mut self) -> Result<()> {
        if self.decompressed.is_none() {
            match self.compression {
                Compression::Lzma2 => {
                    let mut decoder = XzDecoder::new(&self.view[1..]);
                    let mut d = Vec::with_capacity(self.view.len());
                    decoder.read_to_end(&mut d)?;
                    self.decompressed = Some(d);
                }
                Compression::Bzip2 => {
                    todo!("bzip2");
                }
                Compression::Zlib => {
                    todo!("zlib");
                }
                Compression::Zstd => {
                    let out = zstd::stream::decode_all(&self.view[1..])?;
                    self.decompressed = Some(out);
                }
                Compression::None => {}
            }
        }

        if self.blob_list.is_none() {
            match self.compression {
                Compression::Lzma2 | Compression::Bzip2 | Compression::Zlib | Compression::Zstd => {
                    let cur = Cursor::new(self.decompressed.as_ref().unwrap());
                    let blob_list = parse_blob_list(cur, self.extended)?;
                    self.blob_list = Some(blob_list);
                }
                Compression::None => {}
            }
        }

        Ok(())
    }

    fn get_blob(&self, idx: u32) -> Result<&[u8]> {
        match self.blob_list {
            Some(ref list) => {
                let start = list[idx as usize] as usize;
                let n = idx as usize + 1;
                let end = if list.len() > n {
                    list[n] as usize
                } else {
                    self.size as usize
                };

                Ok(match self.compression {
                    Compression::Lzma2
                    | Compression::Bzip2
                    | Compression::Zlib
                    | Compression::Zstd => {
                        // decompressed, so we know this exists
                        &self.decompressed.as_ref().unwrap().as_slice()[start..end]
                    }
                    Compression::None => &self.view[1 + start..1 + end],
                })
            }
            None => Err(Error::MissingBlobList),
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

fn parse_blob_list<T: ReadBytesExt>(mut cur: T, extended: bool) -> Result<Vec<u64>> {
    let mut blob_list = Vec::new();

    // determine the count of blobs, by reading the first offset
    let first = if extended {
        cur.read_u64::<LittleEndian>()?
    } else {
        cur.read_u32::<LittleEndian>()? as u64
    };

    let count = if extended { first / 8 } else { first / 4 };

    blob_list.push(first);

    for _ in 0..(count as usize - 1) {
        if extended {
            blob_list.push(cur.read_u64::<LittleEndian>()?);
        } else {
            blob_list.push(cur.read_u32::<LittleEndian>()? as u64);
        }
    }

    Ok(blob_list)
}
