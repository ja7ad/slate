//! Which records does a reopen after an epoch roll lose?
//!
//! `reopen_after_epoch_roll_preserves_data` reports a key-count shortfall but
//! not *which* keys. A lost tail (newest records missing) and a skipped span
//! (a contiguous older block missing) produce the same count and need opposite
//! fixes, so name the gap before changing anything.

use slate_kv::{Db, KeySource, Options};
use slate_kv_core::config::THETA;

fn opts(dir: &std::path::Path) -> Options {
    let _ = dir;
    Options {
        capacity: 8 * 1024 * 1024,
        n_keys: 4096,
        ..Default::default()
    }
}

fn main() {
    let dir = std::path::Path::new("./target/reopen_probe_db");
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();

    let total = THETA + 512;
    {
        let db = Db::open(dir, KeySource::Bytes([11u8; 32]), opts(dir)).unwrap();
        db.put(b"persisted", b"through_reopen").unwrap();
        for i in 0..total {
            db.put(format!("f{i:06}").as_bytes(), b"y").unwrap();
        }
        println!("before: len={} epoch={}", db.len(), db.epoch());
    }

    let db2 = Db::open(dir, KeySource::Bytes([11u8; 32]), opts(dir)).unwrap();
    println!("after : len={} epoch={}", db2.len(), db2.epoch());

    let mut missing = Vec::new();
    for i in 0..total {
        if db2.get(format!("f{i:06}").as_bytes()).unwrap().is_none() {
            missing.push(i);
        }
    }
    println!("missing count = {}", missing.len());
    if !missing.is_empty() {
        let (lo, hi) = (missing[0], missing[missing.len() - 1]);
        let contiguous = missing.len() as u64 == (hi - lo + 1) as u64;
        println!("missing range = {lo}..={hi}, contiguous = {contiguous}");
        println!("total written = {total}; tail-shaped = {}", hi + 1 == total);
    }
}
