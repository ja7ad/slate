use slate_kv::{Db, KeySource, Options};
use std::env;
use std::path::Path;
use std::process::exit;

fn print_usage() {
    eprintln!("SLATE Database CLI Utility");
    eprintln!();
    eprintln!("USAGE:");
    eprintln!("    slate-kv-cli <SUBCOMMAND> <DB_DIR> [ARGS...]");
    eprintln!();
    eprintln!("SUBCOMMANDS:");
    eprintln!("    get   <db_dir> <key> [hex_key]       Read value for key");
    eprintln!("    put   <db_dir> <key> <val> [hex_key] Write key-value pair and commit");
    eprintln!("    del   <db_dir> <key> [hex_key]       Delete key and commit");
    eprintln!("    stats <db_dir> [hex_key]             Show database statistics");
    eprintln!("    help                                 Show help information");
}

fn parse_key_bytes(hex_or_str: Option<&String>) -> [u8; 32] {
    let mut key = [0x42u8; 32];
    if let Some(s) = hex_or_str {
        if s.len() == 64 {
            if let Ok(bytes) = hex_decode(s) {
                key.copy_from_slice(&bytes);
            }
        }
    }
    key
}

fn hex_decode(s: &str) -> Result<[u8; 32], ()> {
    let mut out = [0u8; 32];
    if s.len() != 64 {
        return Err(());
    }
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| ())?;
    }
    Ok(out)
}

fn open_db(db_path_str: &str, dev_key_arg: Option<&String>) -> Db {
    let db_path = Path::new(db_path_str);
    let dev_key = parse_key_bytes(dev_key_arg);
    Db::open(db_path, KeySource::Bytes(dev_key), Options::default()).unwrap_or_else(|e| {
        eprintln!("Failed to open database '{:?}': {e:?}", db_path);
        exit(1);
    })
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        exit(1);
    }

    let cmd = args[1].as_str();
    match cmd {
        "help" | "-h" | "--help" => {
            print_usage();
        }
        "get" => {
            if args.len() < 4 {
                eprintln!("Error: 'get' requires <db_dir> and <key>");
                exit(1);
            }
            let db = open_db(&args[2], args.get(4));
            let key = args[3].as_bytes();

            match db.get(key) {
                Ok(Some(val)) => {
                    println!("{}", String::from_utf8_lossy(&val));
                }
                Ok(None) => {
                    eprintln!("Key not found");
                    exit(2);
                }
                Err(e) => {
                    eprintln!("Error reading key: {e:?}");
                    exit(1);
                }
            }
        }
        "put" => {
            if args.len() < 5 {
                eprintln!("Error: 'put' requires <db_dir>, <key>, and <val>");
                exit(1);
            }
            let db = open_db(&args[2], args.get(5));
            let key = args[3].as_bytes();
            let val = args[4].as_bytes();

            if let Err(e) = db.put_durable(key, val) {
                eprintln!("Failed to write key: {e:?}");
                exit(1);
            }
            println!("OK");
        }
        "del" => {
            if args.len() < 4 {
                eprintln!("Error: 'del' requires <db_dir> and <key>");
                exit(1);
            }
            let db = open_db(&args[2], args.get(4));
            let key = args[3].as_bytes();

            if let Err(e) = db.delete_durable(key) {
                eprintln!("Failed to delete key: {e:?}");
                exit(1);
            }
            println!("OK");
        }
        "stats" => {
            if args.len() < 3 {
                eprintln!("Error: 'stats' requires <db_dir>");
                exit(1);
            }
            let db = open_db(&args[2], args.get(3));

            println!("=== Persistent Database State ===");
            println!("Path:          {}", args[2]);
            println!("Epoch:         {}", db.epoch());
            println!("Acked Seq:     {}", db.acked_seq());
            println!("Next Seq:      {}", db.next_seq());
            println!("Active Keys:   {}", db.len());
            println!("Security Mode: {:?}", db.security_mode());
            println!();
            println!("=== Current Session Metrics ===");
            let stats = db.stats();
            println!("Commits:       {}", stats.commits);
            println!("Wakes:         {}", stats.wakes);
            println!("User Bytes:    {}", stats.user_bytes);
            println!("GC Bytes:      {}", stats.gc_bytes);
            println!("Parity Bytes:  {}", stats.parity_bytes);
            println!("Ckpt Bytes:    {}", stats.ckpt_bytes);
            println!("Erases:        {}", stats.erases);
        }
        other => {
            eprintln!("Unknown command: '{other}'");
            print_usage();
            exit(1);
        }
    }
}
