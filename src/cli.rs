use std::env;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use tokimo_kvdb::{Database, Options};

fn print_usage() {
    eprintln!(
        r#"Usage: tokimo-kvdb-cli [options] <database-file> <command> [args...]

Options:
  -h, --help       Show this help message
  -v, --version    Show version information

Commands:
  get <key>              Get value by key
  put <key> <value>      Set key-value pair
  delete <key>           Delete key
  list                   List all key-value pairs
  stats                  Show database statistics
  inspect                Show metadata and tree summary
  export <file>          Export all entries to a binary file
  import <file>          Import entries from a binary file
  compact                Compact database (remove deleted entries)
  verify                 Verify metadata, tree, and WAL integrity
"#
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage();
        return;
    }

    let first = &args[1];
    if first == "--version" || first == "-v" {
        println!("tokimo-kvdb-cli version {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if first == "--help" || first == "-h" {
        print_usage();
        return;
    }

    if args.len() < 3 {
        print_usage();
        return;
    }

    let db_path = &args[1];
    let command = &args[2];

    let mut db = match Database::open(db_path, Options::default()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open database: {}", e);
            std::process::exit(1);
        }
    };

    match command.as_str() {
        "get" => {
            if args.len() < 4 {
                eprintln!("Usage: tokimo-kvdb-cli <db> get <key>");
                return;
            }
            match db.get(args[3].as_bytes()) {
                Ok(Some(v)) => println!("{}", String::from_utf8_lossy(&v)),
                Ok(None) => println!("Key not found: {}", args[3]),
                Err(e) => eprintln!("Error: {}", e),
            }
        }
        "put" => {
            if args.len() < 5 {
                eprintln!("Usage: tokimo-kvdb-cli <db> put <key> <value>");
                return;
            }
            match db.put(args[3].as_bytes(), args[4].as_bytes()) {
                Ok(()) => println!("OK"),
                Err(e) => eprintln!("Error: {}", e),
            }
        }
        "delete" => {
            if args.len() < 4 {
                eprintln!("Usage: tokimo-kvdb-cli <db> delete <key>");
                return;
            }
            match db.delete(args[3].as_bytes()) {
                Ok(()) => println!("OK"),
                Err(e) => eprintln!("Error: {}", e),
            }
        }
        "list" => match db.iter() {
            Ok(entries) => {
                for (k, v) in &entries {
                    println!(
                        "{} = {}",
                        String::from_utf8_lossy(k),
                        String::from_utf8_lossy(v)
                    );
                }
                println!("\nTotal: {} entries", entries.len());
            }
            Err(e) => eprintln!("Error: {}", e),
        },
        "stats" => match db.stats() {
            Ok(stats) => {
                println!("Database Statistics:");
                println!("  Pages: {}", stats.page_count);
                println!("  Page Size: {} bytes", stats.page_size);
                println!(
                    "  Database Size: {} bytes ({:.2} MB)",
                    stats.db_size,
                    stats.db_size as f64 / (1024.0 * 1024.0)
                );
            }
            Err(e) => eprintln!("Error: {}", e),
        },
        "inspect" => match db.inspect() {
            Ok(stats) => {
                println!("Database");
                println!("  Pages: {}", stats.page_count);
                println!("  Page Size: {} bytes", stats.page_size);
                println!(
                    "  Database Size: {} bytes ({:.2} MB)",
                    stats.db_size,
                    stats.db_size as f64 / (1024.0 * 1024.0)
                );
                println!("Metadata");
                println!("  Root Page ID: {}", stats.root_page_id);
                println!("  Freelist Head: {}", stats.freelist_page);
                println!("  Freelist Pages: {}", stats.freelist_page_count);
                println!("  Last Page ID: {}", stats.last_page_id);
                println!("  WAL Offset: {}", stats.wal_offset);
                println!("B-Tree");
                println!("  Height: {}", stats.tree_height);
                println!("  Nodes: {}", stats.node_count);
                println!("  Leaf Nodes: {}", stats.leaf_count);
                println!("  Internal Nodes: {}", stats.internal_count);
                println!("  Entries: {}", stats.entry_count);
            }
            Err(e) => eprintln!("Error: {}", e),
        },
        "export" => {
            if args.len() < 4 {
                eprintln!("Usage: tokimo-kvdb-cli <db> export <file>");
                return;
            }
            match File::create(&args[3]) {
                Ok(file) => {
                    let mut writer = BufWriter::new(file);
                    match db.export_to_writer(&mut writer) {
                        Ok(count) => println!("Exported {} entries", count),
                        Err(e) => eprintln!("Error: {}", e),
                    }
                }
                Err(e) => eprintln!("Error creating file: {}", e),
            }
        }
        "import" => {
            if args.len() < 4 {
                eprintln!("Usage: tokimo-kvdb-cli <db> import <file>");
                return;
            }
            match File::open(&args[3]) {
                Ok(file) => {
                    let mut reader = BufReader::new(file);
                    match db.import_from_reader(&mut reader) {
                        Ok(count) => println!("Imported {} entries", count),
                        Err(e) => eprintln!("Error: {}", e),
                    }
                }
                Err(e) => eprintln!("Error opening file: {}", e),
            }
        }
        "compact" => match db.compact() {
            Ok(stats) => {
                println!("Compacted database successfully.");
                println!(
                    "New size: {} pages ({:.2} MB)",
                    stats.page_count,
                    stats.db_size as f64 / (1024.0 * 1024.0)
                );
            }
            Err(e) => eprintln!("Error: {}", e),
        },
        "verify" => match db.verify() {
            Ok(stats) => {
                println!("Verification OK");
                println!("  Tree pages checked: {}", stats.checked_tree_pages);
                println!("  Entries checked: {}", stats.checked_entries);
                println!("  WAL records checked: {}", stats.checked_wal_records);
            }
            Err(e) => eprintln!("Verification FAILED: {}", e),
        },
        other => {
            eprintln!("Unknown command: {}", other);
            print_usage();
        }
    }

    let _ = db.close();
}
