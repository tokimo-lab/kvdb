use rand::Rng;
use std::sync::{Arc, Barrier};
use std::time::Instant;
use tokimo_kvdb::{Database, Options};

const OP_COUNT: usize = 64;
const KEY_SPACE: usize = 64;
const SCAN_REPS: usize = 20;
const SEED: u64 = 0xC0FFEE;

fn put_numbered(db: &mut Database, kid: usize, vid: usize) {
    let k = format!("bench-key-{:08}", kid);
    let v = format!("bench-value-{:012}", vid);
    db.put(k.as_bytes(), v.as_bytes()).unwrap();
}

fn get_numbered(db: &mut Database, kid: usize) -> Option<Vec<u8>> {
    let k = format!("bench-key-{:08}", kid);
    db.get(k.as_bytes()).unwrap()
}

fn delete_numbered(db: &mut Database, kid: usize) {
    let k = format!("bench-key-{:08}", kid);
    db.delete(k.as_bytes()).unwrap();
}

fn insert_range(db: &mut Database, start: usize, count: usize) {
    for i in 0..count {
        put_numbered(db, start + i, start + i);
    }
}

fn open_fresh(root: &str, name: &str) -> Database {
    let p = format!("{}/{}.db", root, name);
    let w = format!("{}.wal", &p);
    std::fs::remove_file(&p).ok();
    std::fs::remove_file(&w).ok();
    Database::open(&p, Options::default()).unwrap()
}

struct BenchResult {
    name: &'static str,
    ops: usize,
    elapsed_ns: u64,
}

fn bench_seq_insert(root: &str) -> BenchResult {
    let mut db = open_fresh(root, "si");
    let t = Instant::now();
    insert_range(&mut db, 0, OP_COUNT);
    let e = t.elapsed().as_nanos() as u64;
    db.close().unwrap();
    BenchResult { name: "sequential-insert", ops: OP_COUNT, elapsed_ns: e }
}

fn bench_rnd_insert(root: &str) -> BenchResult {
    let mut db = open_fresh(root, "ri");
    let mut ids: Vec<usize> = (0..OP_COUNT).collect();
    use rand::seq::SliceRandom;
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::seed_from_u64(SEED);
    ids.shuffle(&mut rng);
    let t = Instant::now();
    for &id in &ids {
        put_numbered(&mut db, id, id);
    }
    let e = t.elapsed().as_nanos() as u64;
    db.close().unwrap();
    BenchResult { name: "random-insert", ops: OP_COUNT, elapsed_ns: e }
}

fn bench_lookup(root: &str) -> BenchResult {
    let mut db = open_fresh(root, "pl");
    insert_range(&mut db, 0, KEY_SPACE);
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::seed_from_u64(SEED ^ 0x1111);
    let t = Instant::now();
    for _ in 0..OP_COUNT {
        let id = rng.gen_range(0..KEY_SPACE);
        let _ = get_numbered(&mut db, id).unwrap();
    }
    let e = t.elapsed().as_nanos() as u64;
    db.close().unwrap();
    BenchResult { name: "point-lookup", ops: OP_COUNT, elapsed_ns: e }
}

fn bench_scan(root: &str) -> BenchResult {
    let mut db = open_fresh(root, "sc");
    insert_range(&mut db, 0, KEY_SPACE);
    let t = Instant::now();
    for _ in 0..SCAN_REPS {
        let c = db.for_each(|_, _| {}).unwrap();
        assert_eq!(c, KEY_SPACE);
    }
    let e = t.elapsed().as_nanos() as u64;
    db.close().unwrap();
    BenchResult { name: "scan", ops: KEY_SPACE * SCAN_REPS, elapsed_ns: e }
}

fn bench_update(root: &str) -> BenchResult {
    let mut db = open_fresh(root, "up");
    insert_range(&mut db, 0, KEY_SPACE);
    let t = Instant::now();
    for i in 0..OP_COUNT {
        put_numbered(&mut db, i % KEY_SPACE, KEY_SPACE + i);
    }
    let e = t.elapsed().as_nanos() as u64;
    db.close().unwrap();
    BenchResult { name: "update", ops: OP_COUNT, elapsed_ns: e }
}

fn bench_delete(root: &str) -> BenchResult {
    let mut db = open_fresh(root, "dl");
    insert_range(&mut db, 0, KEY_SPACE);
    let del = OP_COUNT.min(KEY_SPACE);
    let t = Instant::now();
    for i in 0..del {
        delete_numbered(&mut db, i);
    }
    let e = t.elapsed().as_nanos() as u64;
    db.close().unwrap();
    BenchResult { name: "delete", ops: del, elapsed_ns: e }
}

fn bench_compact(root: &str) -> BenchResult {
    let mut db = open_fresh(root, "cp");
    insert_range(&mut db, 0, KEY_SPACE);
    for i in 0..KEY_SPACE / 2 {
        put_numbered(&mut db, i, KEY_SPACE * 2 + i);
    }
    for i in 0..KEY_SPACE / 4 {
        delete_numbered(&mut db, i * 2);
    }
    let t = Instant::now();
    db.compact().unwrap();
    let e = t.elapsed().as_nanos() as u64;
    db.close().unwrap();
    BenchResult { name: "compact", ops: 1, elapsed_ns: e }
}

// ── Concurrency benchmarks ──────────────────────────────────────────

fn bench_concurrent_reads(root: &str) -> BenchResult {
    let path = format!("{}/conc_read.db", root);
    let wal = format!("{}.wal", &path);
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&wal).ok();

    // Pre-populate
    {
        let mut db = Database::open(&path, Options::default()).unwrap();
        insert_range(&mut db, 0, KEY_SPACE);
        db.close().unwrap();
    }

    let num_threads = 4;
    let reads_per_thread = OP_COUNT;
    let barrier = Arc::new(Barrier::new(num_threads + 1));

    let mut handles = Vec::new();
    for tid in 0..num_threads {
        let p = path.clone();
        let b = barrier.clone();
        handles.push(std::thread::spawn(move || {
            let mut db = Database::open(&p, Options { enable_wal: false }).unwrap();
            use rand::SeedableRng;
            let mut rng = rand::rngs::StdRng::seed_from_u64(SEED + tid as u64);
            b.wait(); // synchronize start
            for _ in 0..reads_per_thread {
                let id = rng.gen_range(0..KEY_SPACE);
                let _ = get_numbered(&mut db, id);
            }
            db.close().unwrap();
        }));
    }

    let t = Instant::now();
    barrier.wait(); // release all threads
    for h in handles {
        h.join().unwrap();
    }
    let e = t.elapsed().as_nanos() as u64;

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&wal).ok();
    BenchResult {
        name: "concurrent-read-4T",
        ops: num_threads * reads_per_thread,
        elapsed_ns: e,
    }
}

fn bench_concurrent_writes(root: &str) -> BenchResult {
    let num_threads = 4;
    let writes_per_thread = OP_COUNT / num_threads;
    let barrier = Arc::new(Barrier::new(num_threads + 1));

    let mut handles = Vec::new();
    for tid in 0..num_threads {
        let r = root.to_string();
        let b = barrier.clone();
        handles.push(std::thread::spawn(move || {
            // Each thread has its own DB file (serialized writes are the realistic model
            // for an embedded DB; parallel writers to separate DBs tests throughput)
            let path = format!("{}/conc_w_{}.db", r, tid);
            let wal = format!("{}.wal", &path);
            std::fs::remove_file(&path).ok();
            std::fs::remove_file(&wal).ok();
            let mut db = Database::open(&path, Options::default()).unwrap();
            b.wait();
            for i in 0..writes_per_thread {
                let kid = tid * writes_per_thread + i;
                put_numbered(&mut db, kid, kid);
            }
            db.close().unwrap();
            std::fs::remove_file(&path).ok();
            std::fs::remove_file(&wal).ok();
        }));
    }

    let t = Instant::now();
    barrier.wait();
    for h in handles {
        h.join().unwrap();
    }
    let e = t.elapsed().as_nanos() as u64;

    BenchResult {
        name: "concurrent-write-4T",
        ops: num_threads * writes_per_thread,
        elapsed_ns: e,
    }
}

fn bench_mixed_rw(root: &str) -> BenchResult {
    let path = format!("{}/mixed_rw.db", root);
    let wal = format!("{}.wal", &path);
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&wal).ok();

    // Pre-populate
    {
        let mut db = Database::open(&path, Options::default()).unwrap();
        insert_range(&mut db, 0, KEY_SPACE);
        db.close().unwrap();
    }

    let reader_threads = 3;
    let reads_per = OP_COUNT;
    let barrier = Arc::new(Barrier::new(reader_threads + 2)); // +1 writer +1 main

    let mut handles = Vec::new();

    // Readers (read-only, own DB handle)
    for tid in 0..reader_threads {
        let p = path.clone();
        let b = barrier.clone();
        handles.push(std::thread::spawn(move || {
            let mut db = Database::open(&p, Options { enable_wal: false }).unwrap();
            use rand::SeedableRng;
            let mut rng = rand::rngs::StdRng::seed_from_u64(SEED + 100 + tid as u64);
            b.wait();
            let mut count = 0usize;
            for _ in 0..reads_per {
                let id = rng.gen_range(0..KEY_SPACE);
                if get_numbered(&mut db, id).is_some() {
                    count += 1;
                }
            }
            db.close().unwrap();
            count
        }));
    }

    // Writer
    {
        let p = path.clone();
        let b = barrier.clone();
        handles.push(std::thread::spawn(move || {
            let mut db = Database::open(&p, Options::default()).unwrap();
            b.wait();
            for i in 0..OP_COUNT {
                put_numbered(&mut db, i % KEY_SPACE, KEY_SPACE * 10 + i);
            }
            db.close().unwrap();
            OP_COUNT
        }));
    }

    let t = Instant::now();
    barrier.wait();
    let mut total_ops = 0usize;
    for h in handles {
        total_ops += h.join().unwrap();
    }
    let e = t.elapsed().as_nanos() as u64;

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&wal).ok();
    BenchResult {
        name: "mixed-rw-3R1W",
        ops: total_ops,
        elapsed_ns: e,
    }
}

fn print_row(r: &BenchResult) {
    let ms = r.elapsed_ns as f64 / 1e6;
    let ops = if r.elapsed_ns == 0 {
        0.0
    } else {
        r.ops as f64 * 1e9 / r.elapsed_ns as f64
    };
    println!("{:<24} {:>10} {:>12.3} {:>14.2}", r.name, r.ops, ms, ops);
}

fn main() {
    let root = "/tmp/tokimo_full_bench";
    std::fs::create_dir_all(root).ok();

    println!("tokimo-kvdb full benchmark suite");
    println!("  ops={} key_space={} scan_reps={} seed=0x{:X}\n",
             OP_COUNT, KEY_SPACE, SCAN_REPS, SEED);

    println!("{:<24} {:>10} {:>12} {:>14}", "workload", "ops", "elapsed ms", "ops/sec");
    println!("{}", "-".repeat(64));

    let results = vec![
        bench_seq_insert(root),
        bench_rnd_insert(root),
        bench_lookup(root),
        bench_scan(root),
        bench_update(root),
        bench_delete(root),
        bench_compact(root),
    ];
    for r in &results {
        print_row(r);
    }

    println!("\n{:<24} {:>10} {:>12} {:>14}", "concurrency", "ops", "elapsed ms", "ops/sec");
    println!("{}", "-".repeat(64));

    let conc = vec![
        bench_concurrent_reads(root),
        bench_concurrent_writes(root),
        bench_mixed_rw(root),
    ];
    for r in &conc {
        print_row(r);
    }

    // Cleanup
    std::fs::remove_dir_all(root).ok();
}
