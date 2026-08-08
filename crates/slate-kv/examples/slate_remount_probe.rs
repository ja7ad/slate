//! Diagnoses index reconstruction across a wrapped log.
//!
//! Writes enough to wrap the region several times, records what is readable
//! before closing, then reopens from flash alone and reports which keys came
//! back. Prints the segment census and replay accounting from both sides so a
//! mismatch can be attributed to the allocator, the span construction, or the
//! tail replay.

use slate_kv::{Db, KeySource, Options, Profile};

const CAPACITY: u32 = 1024 * 1024;
const KEYS: u32 = 16;

fn opts() -> Options {
    Options {
        capacity: CAPACITY,
        b_commit: 4,
        auto_b: false,
        staleness_budget_ms: 1000,
        n_keys: 256,
        profile: Profile::Pi,
        durability: slate_kv::file_flash::Durability::None,
    }
}

fn main() {
    let path = std::path::Path::new("./target/remount_probe_db");
    let _ = std::fs::remove_dir_all(path);
    std::fs::create_dir_all(path).unwrap();

    let val = [b'v'; 64];
    let (before_readable, st_before) = {
        let db = Db::open(path, KeySource::Bytes([0x24; 32]), opts()).unwrap();
        for i in 0..3_000u32 {
            let key = format!("key{:05}", i % KEYS);
            db.put(key.as_bytes(), &val).unwrap();
            if i % 256 == 0 {
                db.commit().unwrap();
            }
        }
        db.commit().unwrap();
        let n = (0..KEYS)
            .filter(|k| {
                let key = format!("key{k:05}");
                matches!(db.get(key.as_bytes()), Ok(Some(v)) if v == val)
            })
            .count();
        (n, db.stats())
    };

    let db = Db::open(path, KeySource::Bytes([0x24; 32]), opts()).unwrap();
    let after_readable = (0..KEYS)
        .filter(|k| {
            let key = format!("key{k:05}");
            matches!(db.get(key.as_bytes()), Ok(Some(v)) if v == val)
        })
        .count();
    let st_after = db.stats();

    println!("readable before close : {before_readable}/{KEYS}");
    println!("readable after remount: {after_readable}/{KEYS}");
    println!();
    println!("BEFORE  hot_head={} cold_head={} seg_end={} segments={} sealed={} free={}",
        st_before.hot_head, st_before.cold_head, st_before.seg_end,
        st_before.segments, st_before.segments_sealed, st_before.segments_free);
    println!("        ckpt_seg_seq={} cur_seg_seq={} ckpt_bytes={}",
        st_before.ckpt_seg_seq, st_before.cur_seg_seq, st_before.ckpt_bytes);
    println!("AFTER   hot_head={} cold_head={} seg_end={} segments={} sealed={} free={}",
        st_after.hot_head, st_after.cold_head, st_after.seg_end,
        st_after.segments, st_after.segments_sealed, st_after.segments_free);
    println!("        ckpt_seg_seq={} cur_seg_seq={}",
        st_after.ckpt_seg_seq, st_after.cur_seg_seq);
}
