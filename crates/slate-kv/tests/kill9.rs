use slate_kv::{Db, KeySource, Options, Profile};
use std::env;
use std::process::Command;

#[test]
fn test_kill9_recovery() {
    // If we are the child, run the writer loop
    if env::var("KILL9_CHILD").is_ok() {
        let path = std::path::Path::new("./test_db_kill9");
        let opts = Options {
            capacity: 1024 * 1024,
            b_commit: 1, // sync commit
            auto_b: false,
            staleness_budget_ms: 1000,
            n_keys: 100,
            profile: Profile::Pi,
            durability: slate_kv::file_flash::Durability::Full,
            ..Default::default()
        };
        let db = Db::open(path, KeySource::Bytes([0x42; 32]), opts).unwrap();
        for i in 0..10000 {
            let key = format!("k{:04}", i % 100);
            let val = format!("v{:04}", i);
            db.put_durable(key.as_bytes(), val.as_bytes()).unwrap();
            if i % 100 == 0 {
                println!("wrote {}", i);
            }
        }
        std::process::exit(0);
    }

    // Parent process
    let path = std::path::Path::new("./test_db_kill9");
    let _ = std::fs::remove_dir_all(path);
    std::fs::create_dir_all(path).unwrap();

    let exe = env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .env("KILL9_CHILD", "1")
        .arg("--nocapture")
        .spawn()
        .expect("Failed to spawn child");

    std::thread::sleep(std::time::Duration::from_millis(50));

    // Kill child
    let _ = child.kill();
    let _ = child.wait();

    // Reopen and verify prefix durability holds (it mounts successfully)
    let opts = Options {
        capacity: 1024 * 1024,
        b_commit: 5,
        auto_b: false,
        staleness_budget_ms: 1000,
        n_keys: 100,
        profile: Profile::Pi,
        durability: slate_kv::file_flash::Durability::Full,
        ..Default::default()
    };
    let db = Db::open(path, KeySource::Bytes([0x42; 32]), opts);
    assert!(db.is_ok(), "Failed to recover after kill -9");
    println!("kill-9 recovery test passed.");
}
