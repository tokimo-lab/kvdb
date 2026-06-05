use crate::constants::*;
use crate::error::*;
use std::fs::{File, OpenOptions};

#[cfg(unix)]
use std::os::unix::fs::FileExt;

/// Helper: pwrite with retry on partial/interrupted writes.
#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], mut offset: u64) -> Result<()> {
    let mut written = 0;
    while written < buf.len() {
        match file.write_at(&buf[written..], offset) {
            Ok(0) => return Err(KvdbError::IoError("write returned 0".into())),
            Ok(n) => {
                written += n;
                offset += n as u64;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Helper: pread with partial read handling.
#[cfg(unix)]
fn read_at_buf(file: &File, buf: &mut [u8], mut offset: u64) -> Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match file.read_at(&mut buf[total..], offset) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                offset += n as u64;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(total)
}

/// Represents a single database page in memory.
pub struct Page {
    pub id: PageId,
    pub data: [u8; PAGE_SIZE],
    pub is_dirty: bool,
}

impl Page {
    pub fn new(id: PageId) -> Self {
        Self {
            id,
            data: [0u8; PAGE_SIZE],
            is_dirty: false,
        }
    }

    pub fn clear(&mut self) {
        self.data = [0u8; PAGE_SIZE];
        self.is_dirty = true;
    }

    pub fn mark_dirty(&mut self) {
        self.is_dirty = true;
    }
}

struct CacheEntry {
    #[allow(dead_code)]
    page_id: PageId,
    page: Box<Page>,
}

/// Page manager responsible for all page-level I/O operations.
pub struct Pager {
    file: File,
    pub page_size: usize,
    cache: Vec<CacheEntry>,
    pub next_page_id: PageId,
}

impl Pager {
    pub fn new(file_path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(file_path)?;

        let file_size = file.metadata()?.len();

        let mut pager = Pager {
            file,
            page_size: PAGE_SIZE,
            cache: Vec::with_capacity(8),
            next_page_id: if file_size == 0 {
                0
            } else {
                file_size / PAGE_SIZE as u64
            },
        };

        if file_size == 0 {
            pager.allocate_metadata_pages()?;
        } else {
            let metadata = pager.read_metadata()?;
            if metadata.is_valid() {
                pager.next_page_id = metadata.last_page_id + 1;
            }
        }

        Ok(pager)
    }

    fn allocate_metadata_pages(&mut self) -> Result<()> {
        let meta_page = self.allocate_page()?;
        debug_assert_eq!(meta_page, META_PAGE_ID);

        let root_page_id = self.allocate_page()?;
        debug_assert_eq!(root_page_id, ROOT_PAGE_ID);

        // Initialize root as empty leaf
        {
            let root = self.get_page_mut(root_page_id)?;
            let header = NodeHeader::new(NodeType::Leaf);
            root.data[..std::mem::size_of::<NodeHeader>()].copy_from_slice(header.as_bytes());
            root.mark_dirty();
        }

        self.write_metadata(&MetaData::new())?;
        self.flush()?;
        Ok(())
    }

    /// Allocate a new page, reusing from freelist if possible.
    pub fn allocate_page(&mut self) -> Result<PageId> {
        let is_bootstrapping = self.next_page_id <= ROOT_PAGE_ID;

        if !is_bootstrapping {
            let metadata = self.read_metadata()?;
            if metadata.freelist_page != INVALID_PAGE_ID {
                let free_page_id = metadata.freelist_page;
                let page = self.get_page(free_page_id)?;
                let next_free = u64::from_le_bytes(page.data[..8].try_into().unwrap());

                let mut new_meta = metadata;
                new_meta.freelist_page = next_free;
                self.write_metadata(&new_meta)?;

                let page = self.get_page_mut(free_page_id)?;
                page.clear();
                return Ok(free_page_id);
            }
        }

        let page_id = self.next_page_id;
        self.next_page_id += 1;

        let mut page = Box::new(Page::new(page_id));
        page.clear();

        self.cache.push(CacheEntry { page_id, page });

        if !is_bootstrapping {
            let mut metadata = self.read_metadata()?;
            metadata.last_page_id = page_id;
            self.write_metadata(&metadata)?;
        }

        Ok(page_id)
    }

    /// Free a page back to the freelist.
    pub fn free_page(&mut self, page_id: PageId) -> Result<()> {
        let metadata = self.read_metadata()?;
        let old_head = metadata.freelist_page;

        let page = self.get_page_mut(page_id)?;
        page.data = [0u8; PAGE_SIZE];
        page.data[..8].copy_from_slice(&old_head.to_le_bytes());
        page.mark_dirty();

        let mut new_meta = metadata;
        new_meta.freelist_page = page_id;
        self.write_metadata(&new_meta)?;
        Ok(())
    }

    pub fn get_page(&mut self, page_id: PageId) -> Result<&Page> {
        self.ensure_cached(page_id)?;
        let entry = self
            .cache
            .iter()
            .find(|e| e.page_id == page_id)
            .ok_or(KvdbError::PageNotFound(page_id))?;
        Ok(&*entry.page)
    }

    pub fn get_page_mut(&mut self, page_id: PageId) -> Result<&mut Page> {
        self.ensure_cached(page_id)?;
        let entry = self
            .cache
            .iter_mut()
            .find(|e| e.page_id == page_id)
            .ok_or(KvdbError::PageNotFound(page_id))?;
        Ok(&mut *entry.page)
    }

    fn ensure_cached(&mut self, page_id: PageId) -> Result<()> {
        if self.cache.iter().any(|e| e.page_id == page_id) {
            return Ok(());
        }
        self.load_page(page_id)
    }

    fn load_page(&mut self, page_id: PageId) -> Result<()> {
        let offset = page_id * PAGE_SIZE as u64;
        let mut page = Box::new(Page::new(page_id));

        let bytes_read = read_at_buf(&self.file, &mut page.data, offset)?;
        if bytes_read < PAGE_SIZE {
            page.data[bytes_read..].fill(0);
        }

        self.cache.push(CacheEntry { page_id, page });
        Ok(())
    }

    pub fn read_metadata(&mut self) -> Result<MetaData> {
        let page = self.get_page(META_PAGE_ID)?;
        Ok(MetaData::from_bytes(&page.data))
    }

    pub fn write_metadata(&mut self, metadata: &MetaData) -> Result<()> {
        let page = self.get_page_mut(META_PAGE_ID)?;
        let meta_bytes = metadata.as_bytes();
        page.data[..meta_bytes.len()].copy_from_slice(meta_bytes);
        page.mark_dirty();
        Ok(())
    }

    pub fn write_node_header(&mut self, page_id: PageId, header: &NodeHeader) -> Result<()> {
        let page = self.get_page_mut(page_id)?;
        let hdr_bytes = header.as_bytes();
        page.data[..hdr_bytes.len()].copy_from_slice(hdr_bytes);
        page.mark_dirty();
        Ok(())
    }

    /// Flush dirty pages using pwrite (no seeking) and single fdatasync.
    pub fn flush(&mut self) -> Result<()> {
        for entry in &mut self.cache {
            if entry.page.is_dirty {
                let offset = entry.page.id * PAGE_SIZE as u64;
                write_all_at(&self.file, &entry.page.data, offset)?;
                entry.page.is_dirty = false;
            }
        }
        self.file.sync_data()?;
        Ok(())
    }

    /// Flush dirty pages WITHOUT fdatasync (caller manages sync).
    pub fn flush_nosync(&mut self) -> Result<()> {
        for entry in &mut self.cache {
            if entry.page.is_dirty {
                let offset = entry.page.id * PAGE_SIZE as u64;
                write_all_at(&self.file, &entry.page.data, offset)?;
                entry.page.is_dirty = false;
            }
        }
        Ok(())
    }

    /// Just fdatasync without writing.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    /// Count freelist pages.
    pub fn count_freelist_pages(&mut self) -> Result<u64> {
        let metadata = self.read_metadata()?;
        let mut count = 0u64;
        let mut current = metadata.freelist_page;
        while current != INVALID_PAGE_ID {
            count += 1;
            let page = self.get_page(current)?;
            current = u64::from_le_bytes(page.data[..8].try_into().unwrap());
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_pager_init_new_db() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        drop(tmp);

        let mut pager = Pager::new(&path).unwrap();
        let meta = pager.read_metadata().unwrap();
        assert!(meta.is_valid());
        assert_eq!({ meta.root_page }, ROOT_PAGE_ID);
        assert_eq!({ meta.freelist_page }, INVALID_PAGE_ID);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_pager_allocate_and_free() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        drop(tmp);

        let mut pager = Pager::new(&path).unwrap();
        let p1 = pager.allocate_page().unwrap();
        let p2 = pager.allocate_page().unwrap();
        assert_ne!(p1, p2);

        pager.free_page(p1).unwrap();
        let p3 = pager.allocate_page().unwrap();
        assert_eq!(p3, p1); // reused from freelist

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_pager_flush_and_reload() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        drop(tmp);

        {
            let mut pager = Pager::new(&path).unwrap();
            let page_id = pager.allocate_page().unwrap();
            let page = pager.get_page_mut(page_id).unwrap();
            page.data[100] = 42;
            page.mark_dirty();
            pager.flush().unwrap();
        }

        {
            let mut pager = Pager::new(&path).unwrap();
            // page 2 is the first user page (0=meta, 1=root)
            let page = pager.get_page(2).unwrap();
            assert_eq!(page.data[100], 42);
        }

        std::fs::remove_file(&path).ok();
    }
}
