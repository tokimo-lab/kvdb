use crate::constants::*;
use crate::error::*;
use crate::pager::{Page, Pager};

/// Key metadata within a B-tree node page.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct KeyInfo {
    key_offset: u16,
    key_len: u16,
    value_offset: u16,
    value_len: u16,
}

const HEADER_SIZE: usize = std::mem::size_of::<NodeHeader>();
const KEY_INFO_SIZE: usize = std::mem::size_of::<KeyInfo>();
const MAX_KEYS: u16 = 64;
const DATA_START_OFFSET: usize = HEADER_SIZE + KEY_INFO_SIZE * MAX_KEYS as usize;

/// Helper to read a NodeHeader from a page.
fn read_header(page: &Page) -> NodeHeader {
    NodeHeader::from_bytes(&page.data[..HEADER_SIZE])
}

/// Helper to write a NodeHeader to a page.
fn write_header(page: &mut Page, header: &NodeHeader) {
    page.data[..HEADER_SIZE].copy_from_slice(header.as_bytes());
    page.mark_dirty();
}

fn read_key_info(page: &Page, index: u16) -> KeyInfo {
    let offset = HEADER_SIZE + index as usize * KEY_INFO_SIZE;
    let bytes = &page.data[offset..offset + KEY_INFO_SIZE];
    unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const KeyInfo) }
}

fn write_key_info(page: &mut Page, index: u16, ki: &KeyInfo) {
    let offset = HEADER_SIZE + index as usize * KEY_INFO_SIZE;
    let bytes = unsafe {
        std::slice::from_raw_parts(ki as *const KeyInfo as *const u8, KEY_INFO_SIZE)
    };
    page.data[offset..offset + KEY_INFO_SIZE].copy_from_slice(bytes);
    page.mark_dirty();
}

fn get_next_data_offset(page: &Page, num_keys: u16) -> usize {
    if num_keys == 0 {
        return DATA_START_OFFSET;
    }
    let mut max_end = DATA_START_OFFSET;
    for i in 0..num_keys {
        let ki = read_key_info(page, i);
        let end = ki.value_offset as usize + ki.value_len as usize;
        if end > max_end {
            max_end = end;
        }
    }
    max_end
}

fn child_page_offset(index: u16) -> usize {
    PAGE_SIZE - std::mem::size_of::<PageId>() * (MAX_KEYS as usize + 1)
        + index as usize * std::mem::size_of::<PageId>()
}

fn get_child_page_id(page: &Page, index: u16) -> PageId {
    let off = child_page_offset(index);
    u64::from_le_bytes(page.data[off..off + 8].try_into().unwrap())
}

fn set_child_page_id(page: &mut Page, index: u16, page_id: PageId) {
    let off = child_page_offset(index);
    page.data[off..off + 8].copy_from_slice(&page_id.to_le_bytes());
    page.mark_dirty();
}

fn get_key_from_page(page: &Page, index: u16) -> Vec<u8> {
    let ki = read_key_info(page, index);
    page.data[ki.key_offset as usize..ki.key_offset as usize + ki.key_len as usize].to_vec()
}

fn get_value_from_page(page: &Page, index: u16) -> Vec<u8> {
    let ki = read_key_info(page, index);
    page.data[ki.value_offset as usize..ki.value_offset as usize + ki.value_len as usize].to_vec()
}

struct SearchResult {
    found: bool,
    index: u16,
}

fn binary_search(page: &Page, num_keys: u16, target: &[u8]) -> SearchResult {
    if num_keys == 0 {
        return SearchResult {
            found: false,
            index: 0,
        };
    }
    let mut low: i32 = 0;
    let mut high: i32 = num_keys as i32 - 1;
    while low <= high {
        let mid = low + (high - low) / 2;
        let ki = read_key_info(page, mid as u16);
        let key =
            &page.data[ki.key_offset as usize..ki.key_offset as usize + ki.key_len as usize];
        match key.cmp(target) {
            std::cmp::Ordering::Equal => {
                return SearchResult {
                    found: true,
                    index: mid as u16,
                }
            }
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid - 1,
        }
    }
    SearchResult {
        found: false,
        index: low as u16,
    }
}

/// Insert a key-value pair into a leaf node page. Returns Ok(true) if inserted, Err if full.
fn leaf_insert(page: &mut Page, key: &[u8], value: &[u8]) -> Result<()> {
    let mut header = read_header(page);
    assert_eq!(header.node_type, NodeType::Leaf);

    let search = binary_search(page, header.num_keys, key);

    if search.found {
        // Update existing — repack data
        return leaf_update(page, &mut header, search.index, key, value);
    }

    if header.num_keys >= MAX_KEYS {
        return Err(KvdbError::NodeFull);
    }

    let data_offset = get_next_data_offset(page, header.num_keys);
    let total_needed = key.len() + value.len();
    let children_zone = PAGE_SIZE - std::mem::size_of::<PageId>() * (MAX_KEYS as usize + 1);
    if data_offset + total_needed > children_zone {
        return Err(KvdbError::NodeFull);
    }

    // Shift key infos to make room
    for i in (search.index..header.num_keys).rev() {
        let ki = read_key_info(page, i);
        write_key_info(page, i + 1, &ki);
    }

    // Write data
    page.data[data_offset..data_offset + key.len()].copy_from_slice(key);
    page.data[data_offset + key.len()..data_offset + key.len() + value.len()]
        .copy_from_slice(value);

    let ki = KeyInfo {
        key_offset: data_offset as u16,
        key_len: key.len() as u16,
        value_offset: (data_offset + key.len()) as u16,
        value_len: value.len() as u16,
    };
    write_key_info(page, search.index, &ki);

    header.num_keys += 1;
    write_header(page, &header);
    Ok(())
}

/// Update an existing key in a leaf node, repacking data.
fn leaf_update(
    page: &mut Page,
    header: &mut NodeHeader,
    index: u16,
    key: &[u8],
    value: &[u8],
) -> Result<()> {
    // Collect all entries
    let num = header.num_keys;
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(num as usize);
    for i in 0..num {
        if i == index {
            entries.push((key.to_vec(), value.to_vec()));
        } else {
            entries.push((get_key_from_page(page, i), get_value_from_page(page, i)));
        }
    }

    // Repack
    let mut offset = DATA_START_OFFSET;
    for (i, (k, v)) in entries.iter().enumerate() {
        page.data[offset..offset + k.len()].copy_from_slice(k);
        page.data[offset + k.len()..offset + k.len() + v.len()].copy_from_slice(v);

        let ki = KeyInfo {
            key_offset: offset as u16,
            key_len: k.len() as u16,
            value_offset: (offset + k.len()) as u16,
            value_len: v.len() as u16,
        };
        write_key_info(page, i as u16, &ki);
        offset += k.len() + v.len();
    }

    // Clear trailing data area
    let children_zone = PAGE_SIZE - std::mem::size_of::<PageId>() * (MAX_KEYS as usize + 1);
    if offset < children_zone {
        page.data[offset..children_zone].fill(0);
    }

    write_header(page, header);
    page.mark_dirty();
    Ok(())
}

/// Delete a key from a leaf node. Returns the deleted value, or KeyNotFound.
fn leaf_delete(page: &mut Page, key: &[u8]) -> Result<Vec<u8>> {
    let mut header = read_header(page);
    assert_eq!(header.node_type, NodeType::Leaf);

    let search = binary_search(page, header.num_keys, key);
    if !search.found {
        return Err(KvdbError::KeyNotFound);
    }

    let deleted_value = get_value_from_page(page, search.index);

    // Collect remaining entries
    let num = header.num_keys;
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity((num - 1) as usize);
    for i in 0..num {
        if i != search.index {
            entries.push((get_key_from_page(page, i), get_value_from_page(page, i)));
        }
    }

    // Repack
    header.num_keys -= 1;
    let mut offset = DATA_START_OFFSET;
    for (i, (k, v)) in entries.iter().enumerate() {
        page.data[offset..offset + k.len()].copy_from_slice(k);
        page.data[offset + k.len()..offset + k.len() + v.len()].copy_from_slice(v);

        let ki = KeyInfo {
            key_offset: offset as u16,
            key_len: k.len() as u16,
            value_offset: (offset + k.len()) as u16,
            value_len: v.len() as u16,
        };
        write_key_info(page, i as u16, &ki);
        offset += k.len() + v.len();
    }

    write_header(page, &header);
    page.mark_dirty();
    Ok(deleted_value)
}

/// B-tree engine operating on a Pager.
pub struct BTree {
    root_page_id: PageId,
}

impl BTree {
    pub fn new(root_page_id: PageId) -> Self {
        Self { root_page_id }
    }

    pub fn root_page_id(&self) -> PageId {
        self.root_page_id
    }

    /// Search for a key, returning the value if found.
    pub fn search(&self, pager: &mut Pager, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.search_in_node(pager, self.root_page_id, key)
    }

    fn search_in_node(
        &self,
        pager: &mut Pager,
        page_id: PageId,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let page = pager.get_page(page_id)?;
        let header = read_header(page);
        let search = binary_search(page, header.num_keys, key);

        match header.node_type {
            NodeType::Leaf => {
                if search.found {
                    Ok(Some(get_value_from_page(page, search.index)))
                } else {
                    Ok(None)
                }
            }
            NodeType::Internal => {
                let child_idx = if search.found {
                    search.index + 1
                } else {
                    search.index
                };
                let child_page_id = get_child_page_id(page, child_idx);
                self.search_in_node(pager, child_page_id, key)
            }
        }
    }

    /// Insert a key-value pair into the B-tree.
    pub fn insert(&mut self, pager: &mut Pager, key: &[u8], value: &[u8]) -> Result<()> {
        if key.is_empty() || key.len() > MAX_KEY_SIZE || value.len() > MAX_VALUE_SIZE {
            return Err(KvdbError::InvalidArgument("key or value size".into()));
        }

        let split = self.insert_into_node(pager, self.root_page_id, key, value)?;

        if let Some((median_key, new_page_id)) = split {
            // Root split: create new root
            let new_root_id = pager.allocate_page()?;
            {
                let new_root = pager.get_page_mut(new_root_id)?;
                let mut header = NodeHeader::new(NodeType::Internal);
                header.num_keys = 1;
                write_header(new_root, &header);

                // Write the median key
                let offset = DATA_START_OFFSET;
                new_root.data[offset..offset + median_key.len()]
                    .copy_from_slice(&median_key);

                let ki = KeyInfo {
                    key_offset: offset as u16,
                    key_len: median_key.len() as u16,
                    value_offset: (offset + median_key.len()) as u16,
                    value_len: 0,
                };
                write_key_info(new_root, 0, &ki);

                set_child_page_id(new_root, 0, self.root_page_id);
                set_child_page_id(new_root, 1, new_page_id);
            }

            self.root_page_id = new_root_id;

            // Update metadata
            let mut metadata = pager.read_metadata()?;
            metadata.root_page = new_root_id;
            pager.write_metadata(&metadata)?;
        }

        Ok(())
    }

    /// Insert into a node, potentially returning a split (median_key, new_right_page).
    fn insert_into_node(
        &mut self,
        pager: &mut Pager,
        page_id: PageId,
        key: &[u8],
        value: &[u8],
    ) -> Result<Option<(Vec<u8>, PageId)>> {
        let page = pager.get_page(page_id)?;
        let header = read_header(page);

        match header.node_type {
            NodeType::Leaf => {
                let page = pager.get_page_mut(page_id)?;
                match leaf_insert(page, key, value) {
                    Ok(()) => Ok(None),
                    Err(KvdbError::NodeFull) => {
                        // Need to split
                        self.split_leaf(pager, page_id, key, value)
                    }
                    Err(e) => Err(e),
                }
            }
            NodeType::Internal => {
                let search = binary_search(page, header.num_keys, key);
                let child_idx = if search.found {
                    // Key exists in an internal node separator; route to right child
                    // which will either update the leaf or find it
                    search.index + 1
                } else {
                    search.index
                };
                let child_page_id = get_child_page_id(page, child_idx);

                let split = self.insert_into_node(pager, child_page_id, key, value)?;

                if let Some((median_key, new_page_id)) = split {
                    self.insert_into_internal(pager, page_id, &median_key, new_page_id)
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn split_leaf(
        &mut self,
        pager: &mut Pager,
        page_id: PageId,
        new_key: &[u8],
        new_value: &[u8],
    ) -> Result<Option<(Vec<u8>, PageId)>> {
        // Collect all entries from this leaf + the new entry
        let page = pager.get_page(page_id)?;
        let header = read_header(page);
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(header.num_keys as usize + 1);

        for i in 0..header.num_keys {
            entries.push((get_key_from_page(page, i), get_value_from_page(page, i)));
        }

        // Insert new entry in sorted position
        let pos = entries
            .binary_search_by(|(k, _)| k.as_slice().cmp(new_key))
            .unwrap_or_else(|p| p);

        // If key exists, update in place
        if pos < entries.len() && entries[pos].0 == new_key {
            entries[pos].1 = new_value.to_vec();
        } else {
            entries.insert(pos, (new_key.to_vec(), new_value.to_vec()));
        }

        let mid = entries.len() / 2;
        let median_key = entries[mid].0.clone();

        // Write left half to original page
        let left_entries = &entries[..mid];
        let right_entries = &entries[mid..];

        // Rewrite left page
        {
            let page = pager.get_page_mut(page_id)?;
            page.data[DATA_START_OFFSET..].fill(0);
            let mut lheader = NodeHeader::new(NodeType::Leaf);
            lheader.num_keys = left_entries.len() as u16;
            write_header(page, &lheader);

            let mut offset = DATA_START_OFFSET;
            for (i, (k, v)) in left_entries.iter().enumerate() {
                page.data[offset..offset + k.len()].copy_from_slice(k);
                page.data[offset + k.len()..offset + k.len() + v.len()].copy_from_slice(v);
                let ki = KeyInfo {
                    key_offset: offset as u16,
                    key_len: k.len() as u16,
                    value_offset: (offset + k.len()) as u16,
                    value_len: v.len() as u16,
                };
                write_key_info(page, i as u16, &ki);
                offset += k.len() + v.len();
            }
        }

        // Allocate right page and write right half
        let right_page_id = pager.allocate_page()?;
        {
            let rpage = pager.get_page_mut(right_page_id)?;
            let mut rheader = NodeHeader::new(NodeType::Leaf);
            rheader.num_keys = right_entries.len() as u16;
            write_header(rpage, &rheader);

            let mut offset = DATA_START_OFFSET;
            for (i, (k, v)) in right_entries.iter().enumerate() {
                rpage.data[offset..offset + k.len()].copy_from_slice(k);
                rpage.data[offset + k.len()..offset + k.len() + v.len()].copy_from_slice(v);
                let ki = KeyInfo {
                    key_offset: offset as u16,
                    key_len: k.len() as u16,
                    value_offset: (offset + k.len()) as u16,
                    value_len: v.len() as u16,
                };
                write_key_info(rpage, i as u16, &ki);
                offset += k.len() + v.len();
            }
        }

        Ok(Some((median_key, right_page_id)))
    }

    fn insert_into_internal(
        &mut self,
        pager: &mut Pager,
        page_id: PageId,
        key: &[u8],
        new_child_page_id: PageId,
    ) -> Result<Option<(Vec<u8>, PageId)>> {
        let page = pager.get_page(page_id)?;
        let header = read_header(page);

        if header.num_keys < MAX_KEYS {
            let page = pager.get_page_mut(page_id)?;
            let mut header = read_header(page);
            let search = binary_search(page, header.num_keys, key);

            let data_offset = get_next_data_offset(page, header.num_keys);
            page.data[data_offset..data_offset + key.len()].copy_from_slice(key);

            // Shift key infos
            for i in (search.index..header.num_keys).rev() {
                let ki = read_key_info(page, i);
                write_key_info(page, i + 1, &ki);
            }

            let ki = KeyInfo {
                key_offset: data_offset as u16,
                key_len: key.len() as u16,
                value_offset: (data_offset + key.len()) as u16,
                value_len: 0,
            };
            write_key_info(page, search.index, &ki);

            // Shift children
            for i in (search.index + 1..=header.num_keys).rev() {
                let cid = get_child_page_id(page, i);
                set_child_page_id(page, i + 1, cid);
            }
            set_child_page_id(page, search.index + 1, new_child_page_id);

            header.num_keys += 1;
            write_header(page, &header);
            Ok(None)
        } else {
            // Need to split internal node
            self.split_internal(pager, page_id, key, new_child_page_id)
        }
    }

    fn split_internal(
        &mut self,
        pager: &mut Pager,
        page_id: PageId,
        new_key: &[u8],
        new_child_page_id: PageId,
    ) -> Result<Option<(Vec<u8>, PageId)>> {
        let page = pager.get_page(page_id)?;
        let header = read_header(page);

        // Collect all keys and children
        let mut keys: Vec<Vec<u8>> = Vec::with_capacity(header.num_keys as usize + 1);
        let mut children: Vec<PageId> = Vec::with_capacity(header.num_keys as usize + 2);

        children.push(get_child_page_id(page, 0));
        for i in 0..header.num_keys {
            keys.push(get_key_from_page(page, i));
            children.push(get_child_page_id(page, i + 1));
        }

        // Insert new key and child
        let pos = keys
            .binary_search_by(|k| k.as_slice().cmp(new_key))
            .unwrap_or_else(|p| p);
        keys.insert(pos, new_key.to_vec());
        children.insert(pos + 1, new_child_page_id);

        let mid = keys.len() / 2;
        let median_key = keys[mid].clone();

        let left_keys = &keys[..mid];
        let left_children = &children[..mid + 1];
        let right_keys = &keys[mid + 1..];
        let right_children = &children[mid + 1..];

        // Rewrite left (original) page
        {
            let page = pager.get_page_mut(page_id)?;
            page.data[DATA_START_OFFSET..].fill(0);

            let mut lheader = NodeHeader::new(NodeType::Internal);
            lheader.num_keys = left_keys.len() as u16;
            write_header(page, &lheader);

            let mut offset = DATA_START_OFFSET;
            for (i, k) in left_keys.iter().enumerate() {
                page.data[offset..offset + k.len()].copy_from_slice(k);
                let ki = KeyInfo {
                    key_offset: offset as u16,
                    key_len: k.len() as u16,
                    value_offset: (offset + k.len()) as u16,
                    value_len: 0,
                };
                write_key_info(page, i as u16, &ki);
                offset += k.len();
            }
            for (i, &cid) in left_children.iter().enumerate() {
                set_child_page_id(page, i as u16, cid);
            }
        }

        // Allocate right page
        let right_page_id = pager.allocate_page()?;
        {
            let rpage = pager.get_page_mut(right_page_id)?;
            let mut rheader = NodeHeader::new(NodeType::Internal);
            rheader.num_keys = right_keys.len() as u16;
            write_header(rpage, &rheader);

            let mut offset = DATA_START_OFFSET;
            for (i, k) in right_keys.iter().enumerate() {
                rpage.data[offset..offset + k.len()].copy_from_slice(k);
                let ki = KeyInfo {
                    key_offset: offset as u16,
                    key_len: k.len() as u16,
                    value_offset: (offset + k.len()) as u16,
                    value_len: 0,
                };
                write_key_info(rpage, i as u16, &ki);
                offset += k.len();
            }
            for (i, &cid) in right_children.iter().enumerate() {
                set_child_page_id(rpage, i as u16, cid);
            }
        }

        Ok(Some((median_key, right_page_id)))
    }

    /// Delete a key from the B-tree.
    pub fn delete(&mut self, pager: &mut Pager, key: &[u8]) -> Result<()> {
        self.delete_from_node(pager, self.root_page_id, key)
    }

    fn delete_from_node(
        &mut self,
        pager: &mut Pager,
        page_id: PageId,
        key: &[u8],
    ) -> Result<()> {
        let page = pager.get_page(page_id)?;
        let header = read_header(page);

        match header.node_type {
            NodeType::Leaf => {
                let page = pager.get_page_mut(page_id)?;
                leaf_delete(page, key)?;
                Ok(())
            }
            NodeType::Internal => {
                let search = binary_search(page, header.num_keys, key);
                let child_idx = if search.found {
                    search.index + 1
                } else {
                    search.index
                };
                let child_page_id = get_child_page_id(page, child_idx);

                // Check if child leaf would underflow
                let child_page = pager.get_page(child_page_id)?;
                let child_header = read_header(child_page);
                if child_header.node_type == NodeType::Leaf && child_header.num_keys <= 1 {
                    return Err(KvdbError::NodeEmpty);
                }

                self.delete_from_node(pager, child_page_id, key)
            }
        }
    }

    /// Iterator that traverses all leaf entries in sorted order.
    pub fn iter_entries(&self, pager: &mut Pager) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut result = Vec::new();
        self.collect_entries(pager, self.root_page_id, &mut result)?;
        Ok(result)
    }

    /// Zero-copy scan: calls the closure for each entry without allocating.
    /// Returns total entry count.
    pub fn for_each_entry<F>(&self, pager: &mut Pager, f: &mut F) -> Result<usize>
    where
        F: FnMut(&[u8], &[u8]),
    {
        let mut count = 0;
        self.for_each_in_node(pager, self.root_page_id, f, &mut count)?;
        Ok(count)
    }

    fn for_each_in_node<F>(
        &self,
        pager: &mut Pager,
        page_id: PageId,
        f: &mut F,
        count: &mut usize,
    ) -> Result<()>
    where
        F: FnMut(&[u8], &[u8]),
    {
        let page = pager.get_page(page_id)?;
        let header = read_header(page);

        match header.node_type {
            NodeType::Leaf => {
                for i in 0..header.num_keys {
                    let ki = read_key_info(page, i);
                    let key = &page.data
                        [ki.key_offset as usize..ki.key_offset as usize + ki.key_len as usize];
                    let value = &page.data[ki.value_offset as usize
                        ..ki.value_offset as usize + ki.value_len as usize];
                    f(key, value);
                    *count += 1;
                }
            }
            NodeType::Internal => {
                let num_children = header.num_keys + 1;
                let mut child_ids = Vec::with_capacity(num_children as usize);
                for i in 0..num_children {
                    child_ids.push(get_child_page_id(page, i));
                }
                for cid in child_ids {
                    self.for_each_in_node(pager, cid, f, count)?;
                }
            }
        }
        Ok(())
    }

    fn collect_entries(
        &self,
        pager: &mut Pager,
        page_id: PageId,
        result: &mut Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<()> {
        let page = pager.get_page(page_id)?;
        let header = read_header(page);

        match header.node_type {
            NodeType::Leaf => {
                for i in 0..header.num_keys {
                    let key = get_key_from_page(page, i);
                    let value = get_value_from_page(page, i);
                    result.push((key, value));
                }
            }
            NodeType::Internal => {
                let num_children = header.num_keys + 1;
                let mut child_ids = Vec::with_capacity(num_children as usize);
                for i in 0..num_children {
                    child_ids.push(get_child_page_id(page, i));
                }
                for cid in child_ids {
                    self.collect_entries(pager, cid, result)?;
                }
            }
        }
        Ok(())
    }

    /// Count the tree height, node counts, etc.
    pub fn inspect(
        &self,
        pager: &mut Pager,
    ) -> Result<(u32, u64, u64, u64, u64)> {
        let mut height = 0u32;
        let mut node_count = 0u64;
        let mut leaf_count = 0u64;
        let mut internal_count = 0u64;
        let mut entry_count = 0u64;
        self.inspect_node(
            pager,
            self.root_page_id,
            1,
            &mut height,
            &mut node_count,
            &mut leaf_count,
            &mut internal_count,
            &mut entry_count,
        )?;
        Ok((height, node_count, leaf_count, internal_count, entry_count))
    }

    fn inspect_node(
        &self,
        pager: &mut Pager,
        page_id: PageId,
        depth: u32,
        height: &mut u32,
        node_count: &mut u64,
        leaf_count: &mut u64,
        internal_count: &mut u64,
        entry_count: &mut u64,
    ) -> Result<()> {
        let page = pager.get_page(page_id)?;
        let header = read_header(page);
        *node_count += 1;

        match header.node_type {
            NodeType::Leaf => {
                *leaf_count += 1;
                *entry_count += header.num_keys as u64;
                if depth > *height {
                    *height = depth;
                }
            }
            NodeType::Internal => {
                *internal_count += 1;
                if depth > *height {
                    *height = depth;
                }
                let num_children = header.num_keys + 1;
                let mut child_ids = Vec::with_capacity(num_children as usize);
                for i in 0..num_children {
                    child_ids.push(get_child_page_id(page, i));
                }
                for cid in child_ids {
                    self.inspect_node(
                        pager,
                        cid,
                        depth + 1,
                        height,
                        node_count,
                        leaf_count,
                        internal_count,
                        entry_count,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Verify the B-tree is well-formed.
    pub fn verify(&self, pager: &mut Pager) -> Result<(u64, u64)> {
        let mut pages_checked = 0u64;
        let mut entries_checked = 0u64;
        self.verify_node(pager, self.root_page_id, None, None, &mut pages_checked, &mut entries_checked)?;
        Ok((pages_checked, entries_checked))
    }

    fn verify_node(
        &self,
        pager: &mut Pager,
        page_id: PageId,
        lower_bound: Option<&[u8]>,
        upper_bound: Option<&[u8]>,
        pages_checked: &mut u64,
        entries_checked: &mut u64,
    ) -> Result<()> {
        let page = pager.get_page(page_id)?;
        let header = read_header(page);
        *pages_checked += 1;

        // Verify keys are in order and within bounds
        let mut prev_key: Option<Vec<u8>> = None;
        for i in 0..header.num_keys {
            let key = get_key_from_page(page, i);

            if let Some(ref pk) = prev_key {
                if key <= *pk {
                    return Err(KvdbError::CorruptedData);
                }
            }
            if let Some(lb) = lower_bound {
                if key.as_slice() < lb {
                    return Err(KvdbError::CorruptedData);
                }
            }
            if let Some(ub) = upper_bound {
                if key.as_slice() >= ub {
                    return Err(KvdbError::CorruptedData);
                }
            }
            *entries_checked += 1;
            prev_key = Some(key);
        }

        if header.node_type == NodeType::Internal {
            let mut child_ids = Vec::new();
            let mut bounds = Vec::new();
            for i in 0..=header.num_keys {
                child_ids.push(get_child_page_id(page, i));
            }
            // Collect keys for bounds
            let mut keys = Vec::new();
            for i in 0..header.num_keys {
                keys.push(get_key_from_page(page, i));
            }
            for i in 0..child_ids.len() {
                let lb = if i == 0 { lower_bound.map(|b| b.to_vec()) } else { Some(keys[i - 1].clone()) };
                let ub = if i == keys.len() { upper_bound.map(|b| b.to_vec()) } else { Some(keys[i].clone()) };
                bounds.push((lb, ub));
            }
            for (i, cid) in child_ids.iter().enumerate() {
                let (ref lb, ref ub) = bounds[i];
                self.verify_node(
                    pager,
                    *cid,
                    lb.as_deref(),
                    ub.as_deref(),
                    pages_checked,
                    entries_checked,
                )?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn setup() -> (String, Pager, BTree) {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        drop(tmp);
        let pager = Pager::new(&path).unwrap();
        let btree = BTree::new(ROOT_PAGE_ID);
        (path, pager, btree)
    }

    #[test]
    fn test_btree_basic_operations() {
        let (path, mut pager, mut btree) = setup();

        btree.insert(&mut pager, b"key1", b"value1").unwrap();
        btree.insert(&mut pager, b"key2", b"value2").unwrap();
        btree.insert(&mut pager, b"key3", b"value3").unwrap();

        assert_eq!(
            btree.search(&mut pager, b"key1").unwrap(),
            Some(b"value1".to_vec())
        );
        assert_eq!(
            btree.search(&mut pager, b"key2").unwrap(),
            Some(b"value2".to_vec())
        );
        assert_eq!(
            btree.search(&mut pager, b"key3").unwrap(),
            Some(b"value3".to_vec())
        );
        assert_eq!(btree.search(&mut pager, b"missing").unwrap(), None);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_btree_root_split() {
        let (path, mut pager, mut btree) = setup();

        // Insert enough entries to trigger a root split
        for i in 0..100u32 {
            let key = format!("key_{:04}", i);
            let val = format!("val_{:04}", i);
            btree.insert(&mut pager, key.as_bytes(), val.as_bytes()).unwrap();
        }

        // All entries should be findable
        for i in 0..100u32 {
            let key = format!("key_{:04}", i);
            let val = format!("val_{:04}", i);
            assert_eq!(
                btree.search(&mut pager, key.as_bytes()).unwrap(),
                Some(val.into_bytes())
            );
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_btree_iterator_sorted_order() {
        let (path, mut pager, mut btree) = setup();

        // Insert in random-ish order
        let keys = vec!["delta", "alpha", "charlie", "bravo", "echo"];
        for k in &keys {
            btree
                .insert(&mut pager, k.as_bytes(), format!("v_{}", k).as_bytes())
                .unwrap();
        }

        let entries = btree.iter_entries(&mut pager).unwrap();
        let collected_keys: Vec<String> = entries
            .iter()
            .map(|(k, _)| String::from_utf8(k.clone()).unwrap())
            .collect();
        assert_eq!(
            collected_keys,
            vec!["alpha", "bravo", "charlie", "delta", "echo"]
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_btree_multi_level_search() {
        let (path, mut pager, mut btree) = setup();

        for i in 0..200u32 {
            let key = format!("k{:05}", i);
            let val = format!("v{:05}", i);
            btree.insert(&mut pager, key.as_bytes(), val.as_bytes()).unwrap();
        }

        for i in 0..200u32 {
            let key = format!("k{:05}", i);
            let val = format!("v{:05}", i);
            assert_eq!(
                btree.search(&mut pager, key.as_bytes()).unwrap(),
                Some(val.into_bytes()),
                "Failed to find {}",
                key
            );
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_btree_repeated_updates() {
        let (path, mut pager, mut btree) = setup();

        btree.insert(&mut pager, b"key", b"version1").unwrap();
        assert_eq!(
            btree.search(&mut pager, b"key").unwrap(),
            Some(b"version1".to_vec())
        );

        btree.insert(&mut pager, b"key", b"version2").unwrap();
        assert_eq!(
            btree.search(&mut pager, b"key").unwrap(),
            Some(b"version2".to_vec())
        );

        btree.insert(&mut pager, b"key", b"version3").unwrap();
        assert_eq!(
            btree.search(&mut pager, b"key").unwrap(),
            Some(b"version3".to_vec())
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_btree_delete() {
        let (path, mut pager, mut btree) = setup();

        btree.insert(&mut pager, b"a", b"1").unwrap();
        btree.insert(&mut pager, b"b", b"2").unwrap();
        btree.insert(&mut pager, b"c", b"3").unwrap();

        btree.delete(&mut pager, b"b").unwrap();
        assert_eq!(btree.search(&mut pager, b"b").unwrap(), None);
        assert_eq!(
            btree.search(&mut pager, b"a").unwrap(),
            Some(b"1".to_vec())
        );
        assert_eq!(
            btree.search(&mut pager, b"c").unwrap(),
            Some(b"3".to_vec())
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_btree_verify() {
        let (path, mut pager, mut btree) = setup();

        for i in 0..50u32 {
            let key = format!("key_{:04}", i);
            let val = format!("val_{:04}", i);
            btree.insert(&mut pager, key.as_bytes(), val.as_bytes()).unwrap();
        }

        let (pages, entries) = btree.verify(&mut pager).unwrap();
        assert!(pages > 0);
        assert_eq!(entries, 50);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_btree_inspect() {
        let (path, mut pager, mut btree) = setup();

        for i in 0..100u32 {
            let key = format!("key_{:04}", i);
            let val = format!("val_{:04}", i);
            btree.insert(&mut pager, key.as_bytes(), val.as_bytes()).unwrap();
        }

        let (height, nodes, leaves, internals, entries) =
            btree.inspect(&mut pager).unwrap();
        assert!(height >= 2);
        assert_eq!(entries, 100);
        assert!(leaves > 0);
        assert!(internals > 0);
        assert_eq!(nodes, leaves + internals);

        std::fs::remove_file(&path).ok();
    }
}
