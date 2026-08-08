//! Timing probe: how long does it take to force epoch seals on the file-backed
//! path? Several tamper attacks need a volume with two populated checkpoint
//! slots, which only happens after Theta records have been written.
//!
//! Run: cargo run --release -p slate-kv --example probe_epoch_cost

use slate_kv::file_flash::Durability;
use slate_kv::{Db, KeySource, Options, Profile};
use slate_kv_core::config::THETA;

fn main() {
    let dir = std::env::temp_dir().join(format!("slate_probe_epoch_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let opts = Options {
        capacity: 8 * 1024 * 1024,
        b_commit: 128,
        auto_b: false,
        staleness_budget_ms: 1_000_000,
        n_keys: 4096,
        profile: Profile::Pi,
        durability: Durability::OsCache,
        ..Default::default()
    };

    let t0 = std::time::Instant::now();
    let db = Db::open(&dir, KeySource::Bytes([5u8; 32]), opts).unwrap();
    println!("open_genesis_s,{:.3}", t0.elapsed().as_secs_f64());
    println!("epoch_after_open,{}", db.epoch());

    // Cycle a bounded key set: the Theta trigger counts appends, not distinct
    // keys, so this advances the epoch without needing a Theta-sized index.
    let t1 = std::time::Instant::now();
    for i in 0..(THETA + 256) {
        db.put(format!("k{:04}", i % 512).as_bytes(), b"v0123456789")
            .unwrap();
    }
    db.commit().unwrap();
    println!("theta_writes_s,{:.3}", t1.elapsed().as_secs_f64());
    println!("epoch_after_theta,{}", db.epoch());
    let st = db.stats();
    println!("ckpt_bytes,{}", st.ckpt_bytes);
    println!("erases,{}", st.erases);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
