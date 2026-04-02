pub const PAGE_SIZE: usize = 4096;
pub type PageId = u64;
pub const INVALID_PAGE_ID: PageId = u64::MAX;
pub const META_PAGE_ID: PageId = 0;
pub const ROOT_PAGE_ID: PageId = 1;
pub const MAX_KEY_SIZE: usize = 1024;
pub const MAX_VALUE_SIZE: usize = PAGE_SIZE / 2;
pub const DB_VERSION: u32 = 1;
pub const MAGIC: u64 = 0x544F4B494D4F4442; // "TOKIMOBD"

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct MetaData {
    pub magic: u64,
    pub version: u32,
    pub page_size: u32,
    pub root_page: PageId,
    pub freelist_page: PageId,
    pub last_page_id: PageId,
    pub wal_offset: u64,
}

impl MetaData {
    pub fn new() -> Self {
        Self {
            magic: MAGIC,
            version: DB_VERSION,
            page_size: PAGE_SIZE as u32,
            root_page: ROOT_PAGE_ID,
            freelist_page: INVALID_PAGE_ID,
            last_page_id: ROOT_PAGE_ID,
            wal_offset: 0,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.magic == MAGIC && self.version == DB_VERSION
    }

    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(self as *const Self as *const u8, std::mem::size_of::<Self>())
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        assert!(bytes.len() >= std::mem::size_of::<Self>());
        unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const Self) }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeType {
    Leaf = 1,
    Internal = 2,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct NodeHeader {
    pub node_type: NodeType,
    pub num_keys: u16,
    pub _reserved: u16,
}

impl NodeHeader {
    pub fn new(node_type: NodeType) -> Self {
        Self {
            node_type,
            num_keys: 0,
            _reserved: 0,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(self as *const Self as *const u8, std::mem::size_of::<Self>())
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        assert!(bytes.len() >= std::mem::size_of::<Self>());
        unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const Self) }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalRecordType {
    Insert = 1,
    Delete = 2,
    Commit = 3,
    Abort = 4,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct WalRecordHeader {
    pub checksum: u32,
    pub record_type: WalRecordType,
    pub key_len: u16,
    pub value_len: u32,
}

impl WalRecordHeader {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        assert!(bytes.len() >= std::mem::size_of::<Self>());
        unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const Self) }
    }
}
