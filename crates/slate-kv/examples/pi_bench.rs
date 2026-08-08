use slate_kv::{Db, KeySource, Options, Profile};
use std::time::Instant;

fn main() {
    let path = std::path::Path::new("./artifacts/pi_bench_db");
    let _ = std::fs::remove_dir_all(path);
    std::fs::create_dir_all(path).unwrap();

    println!("--- SLATE Raspberry Pi Benchmark ---");

    for &b in &[3, 9, 27, 81] {
        let opts = Options {
            capacity: 4096 * 1024, // 4MB
            b_commit: b,
            auto_b: false,
            staleness_budget_ms: 1000,
            n_keys: 1000,
            profile: Profile::Pi,
            durability: slate_kv::file_flash::Durability::OsCache, // os-cache documented as benchmark-only
            ..Default::default()
        };

        let path = path.join(format!("db_b{}", b));
        std::fs::create_dir_all(&path).unwrap();

        let db = Db::open(&path, KeySource::Bytes([0x42; 32]), opts).unwrap();

        let n_ops = 5000;
        let start = Instant::now();

        for i in 0..n_ops {
            let key = format!("k{:04}", i % 1000);
            let val = format!("v{:04}", i);
            db.put(key.as_bytes(), val.as_bytes()).unwrap();
        }

        let elapsed = start.elapsed();
        let stats = db.stats();
        let ops_sec = (n_ops as f64) / elapsed.as_secs_f64();

        println!(
            "B = {:2} | {:.1} ops/s | {} commits",
            b, ops_sec, stats.commits
        );
    }
}
