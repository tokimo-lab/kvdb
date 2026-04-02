use crate::constants::*;
use crate::error::*;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};

#[cfg(unix)]
use std::os::unix::fs::FileExt;

/// Write-Ahead Log for crash recovery.
pub struct Wal {
    file: File,
    file_path: String,
    current_offset: u64,
}

#[derive(Debug)]
pub struct WalRecord {
    pub record_type: WalRecordType,
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
}

impl Wal {
    pub fn new(db_path: &str) -> Result<Self> {
        let wal_path = format!("{}.wal", db_path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&wal_path)?;

        let current_offset = file.metadata()?.len();

        Ok(Self {
            file,
            file_path: wal_path,
            current_offset,
        })
    }

    fn append_record(
        &mut self,
        record_type: WalRecordType,
        key: &[u8],
        value: Option<&[u8]>,
    ) -> Result<()> {
        let value_len = value.map_or(0u32, |v| v.len() as u32);

        let mut header = WalRecordHeader {
            checksum: 0,
            record_type,
            key_len: key.len() as u16,
            value_len,
        };

        // Compute checksum
        let header_bytes = header.as_bytes();
        let checksum_start = std::mem::size_of::<u32>();
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&header_bytes[checksum_start..]);
        hasher.update(key);
        if let Some(v) = value {
            hasher.update(v);
        }
        header.checksum = hasher.finalize();

        // Coalesce into single write using stack buffer for common case
        let header_size = std::mem::size_of::<WalRecordHeader>();
        let total = header_size + key.len() + value_len as usize;

        // Stack buffer for small records (covers most cases)
        let mut stack_buf = [0u8; 2048];
        let use_stack = total <= stack_buf.len();

        if use_stack {
            stack_buf[..header_size].copy_from_slice(header.as_bytes());
            stack_buf[header_size..header_size + key.len()].copy_from_slice(key);
            if let Some(v) = value {
                stack_buf[header_size + key.len()..total].copy_from_slice(v);
            }
            // pwrite — no seek needed
            self.file.write_at(&stack_buf[..total], self.current_offset)?;
        } else {
            let mut buf = Vec::with_capacity(total);
            buf.extend_from_slice(header.as_bytes());
            buf.extend_from_slice(key);
            if let Some(v) = value {
                buf.extend_from_slice(v);
            }
            self.file.write_at(&buf, self.current_offset)?;
        }

        self.current_offset += total as u64;
        Ok(())
    }

    /// Force sync WAL to disk.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    pub fn log_insert(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.append_record(WalRecordType::Insert, key, Some(value))
    }

    pub fn log_delete(&mut self, key: &[u8]) -> Result<()> {
        self.append_record(WalRecordType::Delete, key, None)
    }

    pub fn log_commit(&mut self) -> Result<()> {
        self.append_record(WalRecordType::Commit, &[], None)
    }

    pub fn log_abort(&mut self) -> Result<()> {
        self.append_record(WalRecordType::Abort, &[], None)
    }

    /// Read all records from the WAL.
    pub fn read_all(&mut self) -> Result<Vec<WalRecord>> {
        let mut records = Vec::new();
        let mut offset = 0u64;
        let header_size = std::mem::size_of::<WalRecordHeader>();

        while offset < self.current_offset {
            self.file.seek(SeekFrom::Start(offset))?;

            let mut header_buf = vec![0u8; header_size];
            let n = self.file.read(&mut header_buf)?;
            if n < header_size {
                break;
            }

            let header = WalRecordHeader::from_bytes(&header_buf);

            let mut key = vec![0u8; header.key_len as usize];
            if header.key_len > 0 {
                let n = self.file.read(&mut key)?;
                if n < header.key_len as usize {
                    return Err(KvdbError::WalCorrupted);
                }
            }

            let value = if header.value_len > 0 {
                let mut v = vec![0u8; header.value_len as usize];
                let n = self.file.read(&mut v)?;
                if n < header.value_len as usize {
                    return Err(KvdbError::WalCorrupted);
                }
                Some(v)
            } else {
                None
            };

            // Verify checksum
            let checksum_start = std::mem::size_of::<u32>();
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&header_buf[checksum_start..]);
            hasher.update(&key);
            if let Some(ref v) = value {
                hasher.update(v);
            }
            let computed = hasher.finalize();
            if computed != header.checksum {
                return Err(KvdbError::WalCorrupted);
            }

            offset += header_size as u64 + header.key_len as u64 + header.value_len as u64;

            records.push(WalRecord {
                record_type: header.record_type,
                key,
                value,
            });
        }

        Ok(records)
    }

    /// Clear the WAL.
    pub fn clear(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.current_offset = 0;
        Ok(())
    }

    pub fn file_path(&self) -> &str {
        &self.file_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_wal_basic_operations() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        drop(tmp);
        let wal_file = format!("{}.wal", &path);

        {
            let mut wal = Wal::new(&path).unwrap();
            wal.log_insert(b"key1", b"value1").unwrap();
            wal.log_insert(b"key2", b"value2").unwrap();
            wal.log_delete(b"key1").unwrap();
            wal.log_commit().unwrap();
        }

        {
            let mut wal = Wal::new(&path).unwrap();
            let records = wal.read_all().unwrap();
            assert_eq!(records.len(), 4);
            assert_eq!(records[0].record_type, WalRecordType::Insert);
            assert_eq!(records[0].key, b"key1");
            assert_eq!(records[0].value.as_deref(), Some(b"value1".as_slice()));
            assert_eq!(records[1].record_type, WalRecordType::Insert);
            assert_eq!(records[2].record_type, WalRecordType::Delete);
            assert_eq!(records[3].record_type, WalRecordType::Commit);
        }

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&wal_file).ok();
    }

    #[test]
    fn test_wal_checksum_corruption() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        drop(tmp);
        let wal_file = format!("{}.wal", &path);

        {
            let mut wal = Wal::new(&path).unwrap();
            wal.log_insert(b"key", b"value").unwrap();
        }

        // Corrupt the checksum
        {
            let mut file = OpenOptions::new()
                .write(true)
                .open(&wal_file)
                .unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(&[0xFF, 0xFF, 0xFF, 0xFF]).unwrap();
        }

        {
            let mut wal = Wal::new(&path).unwrap();
            let result = wal.read_all();
            assert!(matches!(result, Err(KvdbError::WalCorrupted)));
        }

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&wal_file).ok();
    }

    #[test]
    fn test_wal_truncated_value() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        drop(tmp);
        let wal_file = format!("{}.wal", &path);

        {
            let mut wal = Wal::new(&path).unwrap();
            wal.log_insert(b"key", b"value").unwrap();
        }

        // Truncate the file
        {
            let file = OpenOptions::new()
                .write(true)
                .open(&wal_file)
                .unwrap();
            let len = file.metadata().unwrap().len();
            file.set_len(len - 2).unwrap();
        }

        {
            let mut wal = Wal::new(&path).unwrap();
            let result = wal.read_all();
            assert!(matches!(result, Err(KvdbError::WalCorrupted)));
        }

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&wal_file).ok();
    }

    #[test]
    fn test_wal_clear() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        drop(tmp);
        let wal_file = format!("{}.wal", &path);

        let mut wal = Wal::new(&path).unwrap();
        wal.log_insert(b"key", b"value").unwrap();
        wal.clear().unwrap();

        let records = wal.read_all().unwrap();
        assert!(records.is_empty());

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&wal_file).ok();
    }
}
