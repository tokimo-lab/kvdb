use rand::seq::SliceRandom;
use rand::Rng;
use std::time::Instant;
use tokimo_kvdb::{Database, Options};

const OPERATION_COUNT: usize = 64;
const SCAN_REPETITIONS: usize = 20;
const KEY_SPACE: usize = 64;
const SEED: u64 = 0xC0FFEE;

struct BenchResult {
    workload: &'static str,
    operations: usize,
    elapsed_ns: u64,
}

fn put_numbered(db: &mut Database, key_id: usize, value_id: usize) {
    let key = format!("bench-key-{:08}", key_id);
    let value = format!("bench-value-{:012}", value_id);
    db.put(key.as_bytes(), value.as_bytes()).unwrap();
}

fn get_numbered(db: &mut Database, key_id: usize) -> Option<Vec<u8>> {
    let key = format!("bench-key-{:08}", key_id);
    db.get(key.as_bytes()).unwrap()
}

fn delete_numbered(db: &mut Database, key_id: usize) {
    let key = format!("bench-key-{:08}", key_id);
    db.delete(key.as_bytes()).unwrap();
}

fn insert_range(db: &mut Database, start: usize, count: usize) {
    for i in 0..count {
        put_numbered(db, start + i, start + i);
    }
}

fn open_bench_db(root: &str, name: &str) -> Database {
    let db_path = format!("{}/{}.db", root, name);
    let wal_path = format!("{}.wal", &db_path);
    std::fs::remove_file(&db_path).ok();
    std::fs::remove_file(&wal_path).ok();
    Database::open(&db_path, Options::default()).unwrap()
}

fn bench_sequential_insert(root: &str) -> BenchResult {
    let mut db = open_bench_db(root, "sequential_insert");
    let start = Instant::now();
    insert_range(&mut db, 0, OPERATION_COUNT);
    let elapsed = start.elapsed().as_nanos() as u64;
    db.close().unwrap();
    BenchResult {
        workload: "sequential-insert",
        operations: OPERATION_COUNT,
        elapsed_ns: elapsed,
    }
}

fn bench_random_insert(root: &str) -> BenchResult {
    let mut db = open_bench_db(root, "random_insert");
    let mut ids: Vec<usize> = (0..OPERATION_COUNT).collect();
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::seed_from_u64(SEED);
    ids.shuffle(&mut rng);

    let start = Instant::now();
    for &id in &ids {
        put_numbered(&mut db, id, id);
    }
    let elapsed = start.elapsed().as_nanos() as u64;
    db.close().unwrap();
    BenchResult {
        workload: "random-insert",
        operations: OPERATION_COUNT,
        elapsed_ns: elapsed,
    }
}

fn bench_point_lookup(root: &str) -> BenchResult {
    let mut db = open_bench_db(root, "point_lookup");
    insert_range(&mut db, 0, KEY_SPACE);

    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::seed_from_u64(SEED ^ 0x1111);

    let start = Instant::now();
    for _ in 0..OPERATION_COUNT {
        let id = rng.gen_range(0..KEY_SPACE);
        let _ = get_numbered(&mut db, id).unwrap();
    }
    let elapsed = start.elapsed().as_nanos() as u64;
    db.close().unwrap();
    BenchResult {
        workload: "point-lookup",
        operations: OPERATION_COUNT,
        elapsed_ns: elapsed,
    }
}

fn bench_scan(root: &str) -> BenchResult {
    let mut db = open_bench_db(root, "scan");
    insert_range(&mut db, 0, KEY_SPACE);

    let start = Instant::now();
    for _ in 0..SCAN_REPETITIONS {
        let count = db.for_each(|_k, _v| {}).unwrap();
        assert_eq!(count, KEY_SPACE);
    }
    let elapsed = start.elapsed().as_nanos() as u64;
    db.close().unwrap();
    BenchResult {
        workload: "scan",
        operations: KEY_SPACE * SCAN_REPETITIONS,
        elapsed_ns: elapsed,
    }
}

fn bench_update(root: &str) -> BenchResult {
    let mut db = open_bench_db(root, "update");
    insert_range(&mut db, 0, KEY_SPACE);

    let start = Instant::now();
    for i in 0..OPERATION_COUNT {
        put_numbered(&mut db, i % KEY_SPACE, KEY_SPACE + i);
    }
    let elapsed = start.elapsed().as_nanos() as u64;
    db.close().unwrap();
    BenchResult {
        workload: "update",
        operations: OPERATION_COUNT,
        elapsed_ns: elapsed,
    }
}

fn bench_delete(root: &str) -> BenchResult {
    let mut db = open_bench_db(root, "delete");
    insert_range(&mut db, 0, KEY_SPACE);

    let delete_count = OPERATION_COUNT.min(KEY_SPACE);
    let start = Instant::now();
    for i in 0..delete_count {
        delete_numbered(&mut db, i);
    }
    let elapsed = start.elapsed().as_nanos() as u64;
    db.close().unwrap();
    BenchResult {
        workload: "delete",
        operations: delete_count,
        elapsed_ns: elapsed,
    }
}

fn bench_compaction(root: &str) -> BenchResult {
    let mut db = open_bench_db(root, "compact");
    insert_range(&mut db, 0, KEY_SPACE);

    for i in 0..KEY_SPACE / 2 {
        put_numbered(&mut db, i, KEY_SPACE * 2 + i);
    }
    for i in 0..KEY_SPACE / 4 {
        delete_numbered(&mut db, i * 2);
    }

    let start = Instant::now();
    db.compact().unwrap();
    let elapsed = start.elapsed().as_nanos() as u64;
    db.close().unwrap();
    BenchResult {
        workload: "compact",
        operations: 1,
        elapsed_ns: elapsed,
    }
}

fn main() {
    let root = "/tmp/tokimo_kvdb_bench";
    std::fs::create_dir_all(root).ok();

    println!("tokimo-kvdb benchmark suite (Rust)");
    println!("  operations: {}", OPERATION_COUNT);
    println!("  scan repetitions: {}", SCAN_REPETITIONS);
    println!("  key space: {}", KEY_SPACE);
    println!("  seed: 0x{:X}\n", SEED);

    let results = vec![
        bench_sequential_insert(root),
        bench_random_insert(root),
        bench_point_lookup(root),
        bench_scan(root),
        bench_update(root),
        bench_delete(root),
        bench_compaction(root),
    ];

    println!(
        "{:<20} {:>12} {:>14} {:>16}",
        "workload", "operations", "elapsed ms", "ops/sec"
    );
    for r in &results {
        let elapsed_ms = r.elapsed_ns as f64 / 1_000_000.0;
        let ops_per_sec = if r.elapsed_ns == 0 {
            0.0
        } else {
            r.operations as f64 * 1_000_000_000.0 / r.elapsed_ns as f64
        };
        println!(
            "{:<20} {:>12} {:>14.3} {:>16.2}",
            r.workload, r.operations, elapsed_ms, ops_per_sec
        );
    }
}
