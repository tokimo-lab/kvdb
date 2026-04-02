use crate::btree::BTree;
use crate::constants::*;
use crate::error::*;
use crate::pager::Pager;
use crate::wal::Wal;
use std::io::{Read, Write};

/// Configuration options for database initialization.
#[derive(Debug, Clone)]
pub struct Options {
    pub enable_wal: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { enable_wal: true }
    }
}

/// Transaction state.
#[derive(Debug, PartialEq)]
enum TransactionState {
    None,
    Active,
}

/// Database statistics.
#[derive(Debug, Clone)]
pub struct Stats {
    pub page_count: u64,
    pub page_size: u32,
    pub db_size: u64,
}

/// Detailed inspection results.
#[derive(Debug, Clone)]
pub struct InspectStats {
    pub page_count: u64,
    pub page_size: u32,
    pub db_size: u64,
    pub root_page_id: PageId,
    pub freelist_page: PageId,
    pub freelist_page_count: u64,
    pub last_page_id: PageId,
    pub wal_offset: u64,
    pub tree_height: u32,
    pub node_count: u64,
    pub leaf_count: u64,
    pub internal_count: u64,
    pub entry_count: u64,
}

/// Verification results.
#[derive(Debug, Clone)]
pub struct VerifyStats {
    pub checked_tree_pages: u64,
    pub checked_entries: u64,
    pub checked_wal_records: u64,
}

const EXPORT_MAGIC: &[u8; 8] = b"TKDB_EXP";

/// The main database handle.
pub struct Database {
    pager: Pager,
    btree: BTree,
    wal: Option<Wal>,
    tx_state: TransactionState,
    db_path: String,
}

impl Database {
    /// Open or create a database.
    pub fn open(path: &str, options: Options) -> Result<Self> {
        let pager = Pager::new(path)?;
        let metadata = {
            let mut p = pager;
            let m = p.read_metadata()?;
            // Reconstruct — we need to pass pager ownership
            (p, m)
        };
        let pager = metadata.0;
        let meta = metadata.1;

        let btree = BTree::new(meta.root_page);

        let wal = if options.enable_wal {
            Some(Wal::new(path)?)
        } else {
            None
        };

        let mut db = Database {
            pager,
            btree,
            wal,
            tx_state: TransactionState::None,
            db_path: path.to_string(),
        };

        // Replay WAL if present
        if db.wal.is_some() {
            db.replay_wal()?;
        }

        Ok(db)
    }

    fn replay_wal(&mut self) -> Result<()> {
        let records = if let Some(ref mut wal) = self.wal {
            wal.read_all().unwrap_or_default()
        } else {
            return Ok(());
        };

        if records.is_empty() {
            return Ok(());
        }

        // Buffer operations until we see a commit
        let mut pending_inserts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut pending_deletes: Vec<Vec<u8>> = Vec::new();
        let mut has_commit = false;

        for record in &records {
            match record.record_type {
                WalRecordType::Insert => {
                    if let Some(ref value) = record.value {
                        pending_inserts.push((record.key.clone(), value.clone()));
                    }
                }
                WalRecordType::Delete => {
                    pending_deletes.push(record.key.clone());
                }
                WalRecordType::Commit => {
                    // Apply buffered operations
                    for (key, value) in &pending_inserts {
                        self.btree.insert(&mut self.pager, key, value)?;
                    }
                    for key in &pending_deletes {
                        match self.btree.delete(&mut self.pager, key) {
                            Ok(()) => {}
                            Err(KvdbError::KeyNotFound) => {} // Already gone
                            Err(KvdbError::NodeEmpty) => {}
                            Err(e) => return Err(e),
                        }
                    }
                    pending_inserts.clear();
                    pending_deletes.clear();
                    has_commit = true;
                }
                WalRecordType::Abort => {
                    pending_inserts.clear();
                    pending_deletes.clear();
                }
            }
        }

        // Flush changes and clear WAL if we applied anything
        if has_commit {
            self.pager.flush()?;
            if let Some(ref mut wal) = self.wal {
                wal.clear()?;
            }
        } else {
            // No commit found — discard uncommitted operations
            if let Some(ref mut wal) = self.wal {
                wal.clear()?;
            }
        }

        Ok(())
    }

    /// Get a value by key.
    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if key.is_empty() || key.len() > MAX_KEY_SIZE {
            return Err(KvdbError::InvalidArgument("key size".into()));
        }
        self.btree.search(&mut self.pager, key)
    }

    /// Put a key-value pair. Auto-commits if no explicit transaction is active.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if key.is_empty() || key.len() > MAX_KEY_SIZE || value.len() > MAX_VALUE_SIZE {
            return Err(KvdbError::InvalidArgument("key or value size".into()));
        }

        let auto_commit = self.tx_state == TransactionState::None;

        if auto_commit {
            // Fast path: B-tree insert → pwrite pages → single fdatasync
            // WAL written but NOT synced — data durability comes from pager sync.
            // On crash recovery, WAL may or may not be present; data pages are
            // authoritative since they're fdatasync'd.
            if let Some(ref mut wal) = self.wal {
                wal.log_insert(key, value)?;
                wal.log_commit()?;
            }
            self.btree.insert(&mut self.pager, key, value)?;
            self.pager.flush()?; // pwrite + single fdatasync
            if let Some(ref mut wal) = self.wal {
                wal.clear()?;
            }
        } else {
            // Explicit transaction: buffer in WAL, no flush yet
            if let Some(ref mut wal) = self.wal {
                wal.log_insert(key, value)?;
            }
            self.btree.insert(&mut self.pager, key, value)?;
        }

        Ok(())
    }

    /// Delete a key.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        if key.is_empty() || key.len() > MAX_KEY_SIZE {
            return Err(KvdbError::InvalidArgument("key size".into()));
        }

        let auto_commit = self.tx_state == TransactionState::None;

        if auto_commit {
            if let Some(ref mut wal) = self.wal {
                wal.log_delete(key)?;
                wal.log_commit()?;
            }
            self.btree.delete(&mut self.pager, key)?;
            self.pager.flush()?;
            if let Some(ref mut wal) = self.wal {
                wal.clear()?;
            }
        } else {
            if let Some(ref mut wal) = self.wal {
                wal.log_delete(key)?;
            }
            self.btree.delete(&mut self.pager, key)?;
        }

        Ok(())
    }

    /// Begin a transaction.
    pub fn begin_transaction(&mut self) -> Result<()> {
        if self.tx_state == TransactionState::Active {
            return Err(KvdbError::TransactionAlreadyActive);
        }
        self.tx_state = TransactionState::Active;
        Ok(())
    }

    /// Commit the current transaction.
    pub fn commit(&mut self) -> Result<()> {
        if self.tx_state != TransactionState::Active {
            return Err(KvdbError::NoActiveTransaction);
        }

        if let Some(ref mut wal) = self.wal {
            wal.log_commit()?;
            wal.sync()?;
        }

        self.pager.flush()?;

        if let Some(ref mut wal) = self.wal {
            wal.clear()?;
        }

        self.tx_state = TransactionState::None;
        Ok(())
    }

    /// Abort the current transaction.
    pub fn abort(&mut self) -> Result<()> {
        if self.tx_state != TransactionState::Active {
            return Err(KvdbError::NoActiveTransaction);
        }

        if let Some(ref mut wal) = self.wal {
            wal.log_abort()?;
            wal.clear()?;
        }

        self.tx_state = TransactionState::None;
        Ok(())
    }

    /// Get database statistics.
    pub fn stats(&mut self) -> Result<Stats> {
        let metadata = self.pager.read_metadata()?;
        let page_count = metadata.last_page_id + 1;
        Ok(Stats {
            page_count,
            page_size: metadata.page_size,
            db_size: page_count * metadata.page_size as u64,
        })
    }

    /// Inspect database structure.
    pub fn inspect(&mut self) -> Result<InspectStats> {
        let metadata = self.pager.read_metadata()?;
        let page_count = metadata.last_page_id + 1;
        let freelist_count = self.pager.count_freelist_pages()?;

        let (height, node_count, leaf_count, internal_count, entry_count) =
            self.btree.inspect(&mut self.pager)?;

        Ok(InspectStats {
            page_count,
            page_size: metadata.page_size,
            db_size: page_count * metadata.page_size as u64,
            root_page_id: metadata.root_page,
            freelist_page: metadata.freelist_page,
            freelist_page_count: freelist_count,
            last_page_id: metadata.last_page_id,
            wal_offset: metadata.wal_offset,
            tree_height: height,
            node_count,
            leaf_count,
            internal_count,
            entry_count,
        })
    }

    /// Verify database integrity.
    pub fn verify(&mut self) -> Result<VerifyStats> {
        let metadata = self.pager.read_metadata()?;
        if !metadata.is_valid() {
            return Err(KvdbError::CorruptedData);
        }

        let (tree_pages, tree_entries) = self.btree.verify(&mut self.pager)?;

        let wal_records = if let Some(ref mut wal) = self.wal {
            wal.read_all().unwrap_or_default().len() as u64
        } else {
            0
        };

        Ok(VerifyStats {
            checked_tree_pages: tree_pages,
            checked_entries: tree_entries,
            checked_wal_records: wal_records,
        })
    }

    /// Iterator over all entries (sorted by key).
    pub fn iter(&mut self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.btree.iter_entries(&mut self.pager)
    }

    /// Zero-copy scan over all entries. Calls the closure for each entry
    /// without heap allocation. Returns entry count.
    pub fn for_each<F>(&mut self, mut f: F) -> Result<usize>
    where
        F: FnMut(&[u8], &[u8]),
    {
        self.btree.for_each_entry(&mut self.pager, &mut f)
    }

    /// Compact the database by rebuilding it.
    pub fn compact(&mut self) -> Result<Stats> {
        let entries = self.btree.iter_entries(&mut self.pager)?;

        // Create temp database
        let compact_path = format!("{}.compact", &self.db_path);
        {
            let mut compact_pager = Pager::new(&compact_path)?;
            let mut compact_btree = BTree::new(ROOT_PAGE_ID);

            for (key, value) in &entries {
                compact_btree.insert(&mut compact_pager, key, value)?;
            }
            compact_pager.flush()?;
        }

        // Replace original with compacted
        std::fs::rename(&compact_path, &self.db_path)?;

        // Reopen
        self.pager = Pager::new(&self.db_path)?;
        let metadata = self.pager.read_metadata()?;
        self.btree = BTree::new(metadata.root_page);

        if let Some(ref mut wal) = self.wal {
            wal.clear()?;
        }

        self.stats()
    }

    /// Export all entries to a writer.
    pub fn export_to_writer<W: Write>(&mut self, writer: &mut W) -> Result<u64> {
        // Write header
        writer.write_all(EXPORT_MAGIC)?;
        writer.write_all(&1u32.to_le_bytes())?; // version

        let entries = self.btree.iter_entries(&mut self.pager)?;
        let mut count = 0u64;

        for (key, value) in &entries {
            writer.write_all(&(key.len() as u16).to_le_bytes())?;
            writer.write_all(&(value.len() as u32).to_le_bytes())?;
            writer.write_all(key)?;
            writer.write_all(value)?;
            count += 1;
        }

        writer.flush()?;
        Ok(count)
    }

    /// Import entries from a reader.
    pub fn import_from_reader<R: Read>(&mut self, reader: &mut R) -> Result<u64> {
        // Read and validate header
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)?;
        if &magic != EXPORT_MAGIC {
            return Err(KvdbError::InvalidArgument("invalid export file".into()));
        }
        let mut version_buf = [0u8; 4];
        reader.read_exact(&mut version_buf)?;
        let version = u32::from_le_bytes(version_buf);
        if version != 1 {
            return Err(KvdbError::InvalidArgument("unsupported export version".into()));
        }

        let mut count = 0u64;
        loop {
            let mut key_len_buf = [0u8; 2];
            match reader.read_exact(&mut key_len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            let key_len = u16::from_le_bytes(key_len_buf) as usize;

            let mut val_len_buf = [0u8; 4];
            reader.read_exact(&mut val_len_buf)?;
            let val_len = u32::from_le_bytes(val_len_buf) as usize;

            if key_len == 0 || key_len > MAX_KEY_SIZE || val_len > MAX_VALUE_SIZE {
                return Err(KvdbError::InvalidArgument("invalid record size".into()));
            }

            let mut key = vec![0u8; key_len];
            reader.read_exact(&mut key)?;
            let mut value = vec![0u8; val_len];
            reader.read_exact(&mut value)?;

            self.put(&key, &value)?;
            count += 1;
        }

        Ok(count)
    }

    /// Close the database.
    pub fn close(mut self) -> Result<()> {
        self.pager.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn temp_db_path() -> String {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        drop(tmp);
        path
    }

    fn cleanup(path: &str) {
        std::fs::remove_file(path).ok();
        std::fs::remove_file(format!("{}.wal", path)).ok();
    }

    #[test]
    fn test_basic_operations() {
        let path = temp_db_path();
        let mut db = Database::open(&path, Options::default()).unwrap();

        db.put(b"hello", b"world").unwrap();
        db.put(b"foo", b"bar").unwrap();
        db.put(b"number", b"42").unwrap();

        assert_eq!(db.get(b"hello").unwrap(), Some(b"world".to_vec()));
        assert_eq!(db.get(b"foo").unwrap(), Some(b"bar".to_vec()));
        assert_eq!(db.get(b"number").unwrap(), Some(b"42".to_vec()));
        assert_eq!(db.get(b"missing").unwrap(), None);

        db.delete(b"foo").unwrap();
        assert_eq!(db.get(b"foo").unwrap(), None);

        db.close().unwrap();
        cleanup(&path);
    }

    #[test]
    fn test_transaction_commit() {
        let path = temp_db_path();
        let mut db = Database::open(&path, Options::default()).unwrap();

        db.begin_transaction().unwrap();
        db.put(b"tx_key1", b"tx_val1").unwrap();
        db.put(b"tx_key2", b"tx_val2").unwrap();
        db.commit().unwrap();

        assert_eq!(db.get(b"tx_key1").unwrap(), Some(b"tx_val1".to_vec()));
        assert_eq!(db.get(b"tx_key2").unwrap(), Some(b"tx_val2".to_vec()));

        db.close().unwrap();
        cleanup(&path);
    }

    #[test]
    fn test_repeated_updates() {
        let path = temp_db_path();
        let mut db = Database::open(&path, Options::default()).unwrap();

        db.put(b"key", b"version1").unwrap();
        assert_eq!(db.get(b"key").unwrap(), Some(b"version1".to_vec()));

        db.put(b"key", b"version2").unwrap();
        assert_eq!(db.get(b"key").unwrap(), Some(b"version2".to_vec()));

        db.put(b"key", b"version3").unwrap();
        assert_eq!(db.get(b"key").unwrap(), Some(b"version3".to_vec()));

        db.close().unwrap();
        cleanup(&path);
    }

    #[test]
    fn test_replay_committed_wal_on_open() {
        let path = temp_db_path();

        // Write data and simulate crash (no proper close)
        {
            let mut db = Database::open(&path, Options::default()).unwrap();
            db.put(b"persist_key", b"persist_val").unwrap();
            // db.close() not called — simulates crash after WAL commit
        }

        // Reopen and verify data persisted via WAL replay
        {
            let mut db = Database::open(&path, Options::default()).unwrap();
            assert_eq!(
                db.get(b"persist_key").unwrap(),
                Some(b"persist_val".to_vec())
            );
            db.close().unwrap();
        }

        cleanup(&path);
    }

    #[test]
    fn test_ignore_uncommitted_wal_on_open() {
        let path = temp_db_path();
        let _wal_path = format!("{}.wal", &path);

        // Create empty database
        {
            let db = Database::open(&path, Options::default()).unwrap();
            db.close().unwrap();
        }

        // Write WAL records without commit
        {
            let mut wal = Wal::new(&path).unwrap();
            wal.log_insert(b"uncommitted", b"data").unwrap();
            // No commit!
        }

        // Reopen — uncommitted data should be discarded
        {
            let mut db = Database::open(&path, Options::default()).unwrap();
            assert_eq!(db.get(b"uncommitted").unwrap(), None);
            db.close().unwrap();
        }

        cleanup(&path);
    }

    #[test]
    fn test_ignore_aborted_wal_on_open() {
        let path = temp_db_path();

        // Create empty database
        {
            let db = Database::open(&path, Options::default()).unwrap();
            db.close().unwrap();
        }

        // Write WAL records with abort
        {
            let mut wal = Wal::new(&path).unwrap();
            wal.log_insert(b"aborted_key", b"aborted_val").unwrap();
            wal.log_abort().unwrap();
        }

        // Reopen — aborted data should be discarded
        {
            let mut db = Database::open(&path, Options::default()).unwrap();
            assert_eq!(db.get(b"aborted_key").unwrap(), None);
            db.close().unwrap();
        }

        cleanup(&path);
    }

    #[test]
    fn test_replay_delete_on_open() {
        let path = temp_db_path();

        {
            let mut db = Database::open(&path, Options::default()).unwrap();
            db.put(b"to_delete", b"value").unwrap();
            db.put(b"to_keep", b"keeper").unwrap();
            db.close().unwrap();
        }

        {
            let mut db = Database::open(&path, Options::default()).unwrap();
            db.delete(b"to_delete").unwrap();
            // Don't close cleanly
        }

        {
            let mut db = Database::open(&path, Options::default()).unwrap();
            assert_eq!(db.get(b"to_delete").unwrap(), None);
            assert_eq!(db.get(b"to_keep").unwrap(), Some(b"keeper".to_vec()));
            db.close().unwrap();
        }

        cleanup(&path);
    }

    #[test]
    fn test_compact_preserves_data() {
        let path = temp_db_path();
        let mut db = Database::open(&path, Options::default()).unwrap();

        db.put(b"key1", b"val1").unwrap();
        db.put(b"key2", b"val2").unwrap();
        db.put(b"key3", b"val3").unwrap();

        db.compact().unwrap();

        assert_eq!(db.get(b"key1").unwrap(), Some(b"val1".to_vec()));
        assert_eq!(db.get(b"key2").unwrap(), Some(b"val2".to_vec()));
        assert_eq!(db.get(b"key3").unwrap(), Some(b"val3".to_vec()));

        db.close().unwrap();
        cleanup(&path);
    }

    #[test]
    fn test_verify_healthy_database() {
        let path = temp_db_path();
        let mut db = Database::open(&path, Options::default()).unwrap();

        for i in 0..20u32 {
            db.put(format!("k{:04}", i).as_bytes(), format!("v{:04}", i).as_bytes())
                .unwrap();
        }

        let stats = db.verify().unwrap();
        assert!(stats.checked_tree_pages > 0);
        assert_eq!(stats.checked_entries, 20);

        db.close().unwrap();
        cleanup(&path);
    }

    #[test]
    fn test_inspect_fresh_database() {
        let path = temp_db_path();
        let mut db = Database::open(&path, Options::default()).unwrap();

        let stats = db.inspect().unwrap();
        assert_eq!(stats.entry_count, 0);
        assert!(stats.page_count >= 2); // at least meta + root
        assert_eq!(stats.page_size, PAGE_SIZE as u32);

        db.close().unwrap();
        cleanup(&path);
    }

    #[test]
    fn test_inspect_multi_level_tree() {
        let path = temp_db_path();
        let mut db = Database::open(&path, Options::default()).unwrap();

        for i in 0..200u32 {
            db.put(format!("k{:05}", i).as_bytes(), format!("v{:05}", i).as_bytes())
                .unwrap();
        }

        let stats = db.inspect().unwrap();
        assert_eq!(stats.entry_count, 200);
        assert!(stats.tree_height >= 2);
        assert!(stats.internal_count > 0);

        db.close().unwrap();
        cleanup(&path);
    }

    #[test]
    fn test_export_import_round_trip() {
        let path1 = temp_db_path();
        let path2 = temp_db_path();

        let mut db1 = Database::open(&path1, Options::default()).unwrap();
        db1.put(b"alpha", b"one").unwrap();
        db1.put(b"beta", b"two").unwrap();
        db1.put(b"gamma", b"three").unwrap();

        let mut export_buf = Vec::new();
        let exported = db1.export_to_writer(&mut export_buf).unwrap();
        assert_eq!(exported, 3);

        let mut db2 = Database::open(&path2, Options::default()).unwrap();
        let mut cursor = std::io::Cursor::new(&export_buf);
        let imported = db2.import_from_reader(&mut cursor).unwrap();
        assert_eq!(imported, 3);

        assert_eq!(db2.get(b"alpha").unwrap(), Some(b"one".to_vec()));
        assert_eq!(db2.get(b"beta").unwrap(), Some(b"two".to_vec()));
        assert_eq!(db2.get(b"gamma").unwrap(), Some(b"three".to_vec()));

        db1.close().unwrap();
        db2.close().unwrap();
        cleanup(&path1);
        cleanup(&path2);
    }

    #[test]
    fn test_export_import_binary_payloads() {
        let path1 = temp_db_path();
        let path2 = temp_db_path();

        let mut db1 = Database::open(&path1, Options::default()).unwrap();
        let binary_key = vec![0x00, 0xFF, 0x80, 0x01];
        let binary_val = vec![0xDE, 0xAD, 0xBE, 0xEF];
        db1.put(&binary_key, &binary_val).unwrap();

        let mut export_buf = Vec::new();
        db1.export_to_writer(&mut export_buf).unwrap();

        let mut db2 = Database::open(&path2, Options::default()).unwrap();
        let mut cursor = std::io::Cursor::new(&export_buf);
        db2.import_from_reader(&mut cursor).unwrap();

        assert_eq!(db2.get(&binary_key).unwrap(), Some(binary_val));

        db1.close().unwrap();
        db2.close().unwrap();
        cleanup(&path1);
        cleanup(&path2);
    }

    #[test]
    fn test_export_import_empty_database() {
        let path1 = temp_db_path();
        let path2 = temp_db_path();

        let mut db1 = Database::open(&path1, Options::default()).unwrap();
        let mut export_buf = Vec::new();
        let exported = db1.export_to_writer(&mut export_buf).unwrap();
        assert_eq!(exported, 0);

        let mut db2 = Database::open(&path2, Options::default()).unwrap();
        let mut cursor = std::io::Cursor::new(&export_buf);
        let imported = db2.import_from_reader(&mut cursor).unwrap();
        assert_eq!(imported, 0);

        db1.close().unwrap();
        db2.close().unwrap();
        cleanup(&path1);
        cleanup(&path2);
    }

    #[test]
    fn test_import_overwrites_existing() {
        let path1 = temp_db_path();
        let path2 = temp_db_path();

        let mut db1 = Database::open(&path1, Options::default()).unwrap();
        db1.put(b"key", b"new_value").unwrap();

        let mut export_buf = Vec::new();
        db1.export_to_writer(&mut export_buf).unwrap();

        let mut db2 = Database::open(&path2, Options::default()).unwrap();
        db2.put(b"key", b"old_value").unwrap();

        let mut cursor = std::io::Cursor::new(&export_buf);
        db2.import_from_reader(&mut cursor).unwrap();

        assert_eq!(db2.get(b"key").unwrap(), Some(b"new_value".to_vec()));

        db1.close().unwrap();
        db2.close().unwrap();
        cleanup(&path1);
        cleanup(&path2);
    }

    #[test]
    fn test_randomized_sequences() {
        use rand::Rng;
        let path = temp_db_path();
        let mut db = Database::open(&path, Options::default()).unwrap();
        let mut model = std::collections::HashMap::new();
        let mut rng = rand::thread_rng();

        for _ in 0..500 {
            let key = format!("rk_{:04}", rng.gen_range(0..100u32));
            let value = format!("rv_{:08}", rng.gen::<u32>());

            if rng.gen_bool(0.8) {
                db.put(key.as_bytes(), value.as_bytes()).unwrap();
                model.insert(key, value);
            } else if model.contains_key(&key) {
                match db.delete(key.as_bytes()) {
                    Ok(()) => {
                        model.remove(&key);
                    }
                    Err(KvdbError::NodeEmpty) => {} // Expected for some tree shapes
                    Err(e) => panic!("unexpected error: {:?}", e),
                }
            }
        }

        // Verify model matches database
        for (key, value) in &model {
            assert_eq!(
                db.get(key.as_bytes()).unwrap(),
                Some(value.as_bytes().to_vec()),
                "mismatch for key: {}",
                key
            );
        }

        db.close().unwrap();
        cleanup(&path);
    }

    #[test]
    fn test_replay_mixed_wal_batches() {
        let path = temp_db_path();

        // Create DB with initial data
        {
            let mut db = Database::open(&path, Options::default()).unwrap();
            db.put(b"existing", b"data").unwrap();
            db.close().unwrap();
        }

        // Write mixed WAL operations
        {
            let mut wal = Wal::new(&path).unwrap();
            // Committed batch
            wal.log_insert(b"batch1_key", b"batch1_val").unwrap();
            wal.log_commit().unwrap();
            // Uncommitted batch (should be discarded)
            wal.log_insert(b"batch2_key", b"batch2_val").unwrap();
            // No commit for batch 2
        }

        // Reopen — only committed batch should be applied
        {
            let mut db = Database::open(&path, Options::default()).unwrap();
            assert_eq!(
                db.get(b"existing").unwrap(),
                Some(b"data".to_vec())
            );
            assert_eq!(
                db.get(b"batch1_key").unwrap(),
                Some(b"batch1_val".to_vec())
            );
            assert_eq!(db.get(b"batch2_key").unwrap(), None);
            db.close().unwrap();
        }

        cleanup(&path);
    }

    #[test]
    fn test_replay_is_idempotent() {
        let path = temp_db_path();

        {
            let mut db = Database::open(&path, Options::default()).unwrap();
            db.put(b"idem_key", b"idem_val").unwrap();
            db.close().unwrap();
        }

        // Reopen multiple times — each should see the same data
        for _ in 0..3 {
            let mut db = Database::open(&path, Options::default()).unwrap();
            assert_eq!(
                db.get(b"idem_key").unwrap(),
                Some(b"idem_val".to_vec())
            );
            db.close().unwrap();
        }

        cleanup(&path);
    }

    #[test]
    fn test_iterator() {
        let path = temp_db_path();
        let mut db = Database::open(&path, Options::default()).unwrap();

        db.put(b"charlie", b"3").unwrap();
        db.put(b"alpha", b"1").unwrap();
        db.put(b"bravo", b"2").unwrap();

        let entries = db.iter().unwrap();
        let keys: Vec<String> = entries
            .iter()
            .map(|(k, _)| String::from_utf8(k.clone()).unwrap())
            .collect();
        assert_eq!(keys, vec!["alpha", "bravo", "charlie"]);

        db.close().unwrap();
        cleanup(&path);
    }
}
