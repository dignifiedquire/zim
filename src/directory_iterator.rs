use crate::directory_entry::DirectoryEntry;
use crate::errors::Result;
use crate::zim::Zim;

pub struct DirectoryIterator<'a> {
    max: u32,
    next: u32,
    zim: &'a Zim,
}

impl<'a> DirectoryIterator<'a> {
    pub fn new(zim: &'a Zim) -> DirectoryIterator<'a> {
        DirectoryIterator {
            max: zim.header.article_count,
            next: 0,
            zim,
        }
    }
}

impl<'a> std::iter::Iterator for DirectoryIterator<'a> {
    type Item = Result<DirectoryEntry>;

    /// Yields every entry in path order.
    ///
    /// A failing entry is reported as `Some(Err(_))` and iteration continues with the next index.
    /// Entries are addressed through independent pointers, so one unparsable entry says nothing
    /// about its successors - returning `None` here would silently truncate the archive.
    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.max {
            return None;
        }

        let idx = self.next;
        self.next += 1;

        Some(self.zim.get_by_url_index(idx))
    }
}
