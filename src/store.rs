use std::borrow::Cow;
use std::cmp::Ordering;
use std::fs::File;
use std::path::{Path, PathBuf};

use memmap::Mmap;

use crate::errors::{Error, Result};

/// A ZIM archive's bytes, which may be spread over several chunk files.
///
/// Archives too large for the target filesystem are split into chunks named `foobar.zimaa`,
/// `foobar.zimab`, and so on. Every offset in the format addresses the concatenation of those
/// chunks, so this presents them as a single address space.
pub struct Store {
    parts: Vec<Part>,
    len: u64,
}

struct Part {
    /// Offset of this part's first byte within the archive as a whole.
    start: u64,
    map: Mmap,
}

impl Part {
    fn end(&self) -> u64 {
        self.start + self.map.len() as u64
    }
}

impl Store {
    /// Opens an archive, falling back to its chunk files when `path` itself does not exist.
    ///
    /// A whole archive takes precedence over chunks of the same name. `path` may also name the
    /// first chunk directly, as `foobar.zimaa`.
    pub fn open(path: &Path) -> Result<Store> {
        // A path naming the first chunk always means the chunked archive - that file exists, but
        // opening it on its own would yield only the archive's first slice.
        let base = chunk_base(path);
        let names_first_chunk = base != path;

        if !names_first_chunk && path.is_file() {
            return Store::from_maps(vec![map_file(path)?]);
        }

        let mut maps = Vec::new();
        for suffix in chunk_suffixes() {
            let mut name = base.clone().into_os_string();
            name.push(suffix);

            let chunk = PathBuf::from(name);
            if !chunk.is_file() {
                // Chunks are consecutive, so the first gap is the end of the archive.
                break;
            }

            maps.push(map_file(&chunk)?);
        }

        if maps.is_empty() {
            // Nothing under either name: surface the error for what the caller actually asked for.
            File::open(path)?;
            return Err(Error::OutOfBounds);
        }

        Store::from_maps(maps)
    }

    fn from_maps(maps: Vec<Mmap>) -> Result<Store> {
        let mut parts = Vec::with_capacity(maps.len());
        let mut len = 0u64;

        for map in maps {
            // An empty chunk would occupy an empty range, which no offset can land in.
            if map.is_empty() {
                continue;
            }

            let size = map.len() as u64;
            parts.push(Part { start: len, map });
            len += size;
        }

        if parts.is_empty() {
            return Err(Error::InvalidHeader("archive is empty"));
        }

        Ok(Store { parts, len })
    }

    /// Total size of the archive, across all chunks.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns `len` bytes starting at `start`.
    ///
    /// Borrows straight from the mapping when the range lies within one chunk - which is always
    /// the case for a single-file archive. A range straddling a chunk boundary has no contiguous
    /// backing, so it is copied instead.
    pub fn slice(&self, start: u64, len: u64) -> Result<Cow<'_, [u8]>> {
        let end = start.checked_add(len).ok_or(Error::OutOfBounds)?;
        if end > self.len {
            return Err(Error::OutOfBounds);
        }

        let idx = self.part_containing(start)?;
        let part = &self.parts[idx];

        if end <= part.end() {
            let from = usize::try_from(start - part.start)?;
            let to = usize::try_from(end - part.start)?;
            return Ok(Cow::Borrowed(&part.map[from..to]));
        }

        let mut out = Vec::with_capacity(usize::try_from(len)?);
        let mut pos = start;

        for part in &self.parts[idx..] {
            if pos >= end {
                break;
            }

            let from = usize::try_from(pos - part.start)?;
            let to = usize::try_from(end.min(part.end()) - part.start)?;
            out.extend_from_slice(&part.map[from..to]);
            pos = part.end();
        }

        Ok(Cow::Owned(out))
    }

    /// Returns up to `len` bytes starting at `start`, stopping at the end of the archive.
    ///
    /// Used where the length of a structure is not known up front and is instead determined by
    /// parsing it.
    pub fn slice_upto(&self, start: u64, len: u64) -> Result<Cow<'_, [u8]>> {
        if start > self.len {
            return Err(Error::OutOfBounds);
        }

        self.slice(start, len.min(self.len - start))
    }

    /// Reads `start..end` as a stream, without materialising it.
    ///
    /// Used for compressed cluster payloads, which are fed straight to a decoder - a cluster can
    /// be tens of megabytes, and copying it in order to decompress it would defeat the mapping.
    pub fn reader(&self, start: u64, end: u64) -> StoreReader<'_> {
        StoreReader {
            store: self,
            pos: start,
            end: end.min(self.len),
        }
    }

    /// The mapped chunks covering the first `upto` bytes, in order.
    pub fn prefix_chunks(&self, upto: u64) -> Vec<&[u8]> {
        let mut out = Vec::new();

        for part in &self.parts {
            if part.start >= upto {
                break;
            }

            let end = usize::try_from(upto.min(part.end()) - part.start).unwrap_or(part.map.len());
            out.push(&part.map[..end]);
        }

        out
    }

    fn part_containing(&self, offset: u64) -> Result<usize> {
        self.parts
            .binary_search_by(|part| {
                if offset < part.start {
                    Ordering::Greater
                } else if offset >= part.end() {
                    Ordering::Less
                } else {
                    Ordering::Equal
                }
            })
            .map_err(|_| Error::OutOfBounds)
    }
}

/// Streams a byte range of the archive, crossing chunk boundaries as needed.
pub struct StoreReader<'a> {
    store: &'a Store,
    pos: u64,
    end: u64,
}

impl std::io::Read for StoreReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.end || buf.is_empty() {
            return Ok(0);
        }

        let idx = self
            .store
            .part_containing(self.pos)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::UnexpectedEof))?;
        let part = &self.store.parts[idx];

        // One part per call; `Read` is free to return a short read, and the caller loops.
        let available = self.end.min(part.end()) - self.pos;
        let len = available.min(buf.len() as u64) as usize;
        let from = (self.pos - part.start) as usize;

        buf[..len].copy_from_slice(&part.map[from..from + len]);
        self.pos += len as u64;

        Ok(len)
    }
}

/// The base name chunks are built from.
///
/// `path` may name the first chunk rather than the archive, in which case the two-letter suffix
/// is stripped back off.
fn chunk_base(path: &Path) -> PathBuf {
    match path.to_str() {
        Some(name) if name.ends_with(".zimaa") => PathBuf::from(&name[..name.len() - 2]),
        _ => path.to_path_buf(),
    }
}

/// `aa`, `ab`, ... `zz`, in the order chunks are numbered.
fn chunk_suffixes() -> impl Iterator<Item = String> {
    (b'a'..=b'z').flat_map(|first| {
        (b'a'..=b'z').map(move |second| format!("{}{}", first as char, second as char))
    })
}

fn map_file(path: &Path) -> Result<Mmap> {
    let file = File::open(path)?;
    Ok(unsafe { Mmap::map(&file)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_suffixes_are_ordered_and_complete() {
        let suffixes: Vec<String> = chunk_suffixes().collect();

        assert_eq!(suffixes.len(), 26 * 26);
        assert_eq!(suffixes[0], "aa");
        assert_eq!(suffixes[1], "ab");
        assert_eq!(suffixes[26], "ba");
        assert_eq!(suffixes[26 * 26 - 1], "zz");
    }

    #[test]
    fn chunk_base_strips_a_first_chunk_name() {
        // Both spellings must resolve to the same set of chunks.
        assert_eq!(chunk_base(Path::new("foo.zimaa")), PathBuf::from("foo.zim"));
        assert_eq!(chunk_base(Path::new("foo.zim")), PathBuf::from("foo.zim"));
    }
}
