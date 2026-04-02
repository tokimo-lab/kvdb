# tokimo-kvdb

A high-performance embedded key-value database written in Rust. Designed for applications that need fast, reliable local storage with minimal overhead.

## Features

- **B-tree indexed storage** — O(log n) lookups, inserts, and deletes
- **Write-Ahead Log (WAL)** — crash recovery with CRC32 integrity checks
- **Page-based I/O** — 4KB page architecture with `pwrite`/`pread` for zero-seek I/O
- **Transactions** — explicit begin/commit/abort with WAL-backed durability
- **Compaction** — reclaim free pages and rebuild the tree
- **Export / Import** — portable binary format for backup and migration
- **CLI tool** — built-in command-line interface for database operations
- **Thread-safe reads** — concurrent read access via separate handles
- **Tiny footprint** — ~600 KB RSS, 26 KB heap usage at runtime

## Performance

Benchmarked with 64 operations per workload, 3-round median, on Linux x86_64:

| Workload | Elapsed (ms) | Ops/sec |
|----------|-------------|---------|
| Sequential Insert | 138 | 465 |
| Random Insert | 140 | 458 |
| Point Lookup | 0.012 | 5,068,504 |
| Scan (1280 entries) | 0.003 | 459,770,115 |
| Update | 142 | 450 |
| Delete | 145 | 445 |
| Compact | 7.5 | 133 |

### Concurrency (4 threads)

| Workload | Total Ops | Elapsed (ms) | Ops/sec |
|----------|-----------|-------------|---------|
| Concurrent Read (4T) | 256 | 3.4 | 74,423 |
| Concurrent Write (4T) | 64 | 151 | 423 |
| Mixed R/W (3R + 1W) | 256 | 153 | 1,669 |

### Resource Usage

| Metric | Value |
|--------|-------|
| Peak RSS (musl static) | 592 KB |
| Heap usage | ~26 KB |
| Binary size (stripped) | 569 KB |
| Build time (clean) | 2.7s |

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
tokimo-kvdb = { git = "https://github.com/tokimo-lab/kvdb" }
```

### Basic Usage

```rust
use tokimo_kvdb::{Database, Options};

fn main() -> tokimo_kvdb::Result<()> {
    let mut db = Database::open("my.db", Options::default())?;

    // Put
    db.put(b"hello", b"world")?;

    // Get
    if let Some(value) = db.get(b"hello")? {
        println!("got: {}", String::from_utf8_lossy(&value));
    }

    // Delete
    db.delete(b"hello")?;

    // Iterate all entries
    db.for_each(|key, value| {
        println!("{}: {}", String::from_utf8_lossy(key), String::from_utf8_lossy(value));
        Ok(())
    })?;

    db.close()?;
    Ok(())
}
```

### Transactions

```rust
use tokimo_kvdb::{Database, Options};

fn main() -> tokimo_kvdb::Result<()> {
    let mut db = Database::open("my.db", Options::default())?;

    db.begin()?;
    db.put(b"key1", b"val1")?;
    db.put(b"key2", b"val2")?;
    db.commit()?;  // atomically durable

    // Or abort to discard
    db.begin()?;
    db.put(b"key3", b"val3")?;
    db.abort()?;   // key3 is not persisted

    db.close()?;
    Ok(())
}
```

### Export & Import

```rust
use tokimo_kvdb::{Database, Options};

fn main() -> tokimo_kvdb::Result<()> {
    let mut db = Database::open("my.db", Options::default())?;

    // Export to portable binary file
    db.export("backup.tkdb")?;

    // Import from backup (overwrites current data)
    db.import("backup.tkdb")?;

    db.close()?;
    Ok(())
}
```

### Maintenance

```rust
use tokimo_kvdb::{Database, Options};

fn main() -> tokimo_kvdb::Result<()> {
    let mut db = Database::open("my.db", Options::default())?;

    // Compact: rebuild tree and reclaim free pages
    db.compact()?;

    // Verify integrity
    let result = db.verify()?;
    println!("pages verified: {}, depth: {}", result.pages_verified, result.tree_depth);

    // Inspect tree structure
    let stats = db.inspect()?;
    println!("entries: {}, pages: {}", stats.total_entries, stats.total_pages);

    db.close()?;
    Ok(())
}
```

## CLI

```bash
cargo install --path . --bin tokimo-kvdb-cli

tokimo-kvdb-cli my.db put mykey myvalue
tokimo-kvdb-cli my.db get mykey
tokimo-kvdb-cli my.db delete mykey
tokimo-kvdb-cli my.db list
tokimo-kvdb-cli my.db stats
tokimo-kvdb-cli my.db compact
tokimo-kvdb-cli my.db verify
tokimo-kvdb-cli my.db inspect
tokimo-kvdb-cli my.db export backup.tkdb
tokimo-kvdb-cli my.db import backup.tkdb
```

## Architecture

```
┌──────────────────────────────────────┐
│            Database API              │
│   get / put / delete / for_each      │
│   begin / commit / abort             │
│   compact / export / import          │
├──────────────────────────────────────┤
│   B-Tree Engine (MAX_KEYS=64)        │
│   binary search / split / merge      │
├──────────┬───────────────────────────┤
│   WAL    │      Page Cache           │
│  (CRC32) │   pwrite / pread          │
├──────────┴───────────────────────────┤
│          File System (4KB pages)     │
│   Page 0: Metadata                   │
│   Page 1: Root B-tree node           │
│   Page 2+: Data / internal nodes     │
└──────────────────────────────────────┘
```

- **Page 0** — Metadata: magic bytes, root page ID, freelist head, page count
- **B-tree** — Keys stored inline in 4KB leaf nodes; internal nodes hold child pointers
- **WAL** — Append-only log with CRC32 per record; replayed on open if committed
- **Freelist** — Singly-linked list of recycled pages stored in page headers

## Key Optimizations

- **`pwrite`/`pread`** — eliminates all `lseek` syscalls via `FileExt` positional I/O
- **Single `fdatasync` per write** — WAL + B-tree + page flush coalesced into one sync call
- **Stack-buffered WAL** — 2KB stack buffer avoids heap allocation for typical records
- **Zero-copy scan** — `for_each` iterates entries without copying keys/values

## Running Benchmarks

```bash
# Standard benchmark
cargo run --release --example benchmark

# Full benchmark with concurrency tests
cargo run --release --example full_benchmark

# Minimal memory footprint (musl static build)
cargo build --release --target x86_64-unknown-linux-musl --example benchmark
```

## Testing

```bash
cargo test --release
```

34 tests covering WAL replay, B-tree operations, transactions, export/import, compaction, verification, and randomized sequences.

## License

MIT — see [LICENSE](LICENSE).
