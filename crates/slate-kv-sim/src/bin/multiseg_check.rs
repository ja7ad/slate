//! Does the REAL engine (`slate_kv::Db`, segment-aware `recover_spans`) survive
//! a remount with more than one segment of data, where the `sim_db` harness
//! (flat `recover`) caps out at SEG_DATA_BYTES?
//!
//! If the real engine passes here, the ceiling `geometry_diag` measured is a
//! staleness bug in the simulator harness, not an engine defect -- and the
//! geometry sweep has to be re-run through a span-aware path before any of its
//! rows can be trusted.

use slate_kv::db::{Db, KeySource, Options, Profile};
use slate_kv::file_flash::Durability;

fn val_for(i: usize) -> Vec<u8> {
    let mut v = vec![0u8; 40];
    v[0] = i as u8;
    v[1] = (i >> 8) as u8;
    v
}

fn main() {
    let dir = std::env::temp_dir().join(format!("slate_multiseg_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let capacity: u32 = 4 << 20;
    let opts = Options {
        capacity,
        b_commit: 1,
        auto_b: false,
        staleness_budget_ms: 1000,
        n_keys: 256,
        profile: Profile::Esp32,
        durability: Durability::None,
        // 256 B page / 4 KiB block: the canonical SPI NOR geometry.
        ..Default::default()
    };

    // FileFlash is 256 B pages / 4 KiB blocks, so with b_commit=1 each record
    // costs 4 pages = 1024 B. 96 records = 98 304 B = three segments' worth of
    // data area, comfortably past the 32 768 B one-segment ceiling.
    let n = 96usize;
    let per_record = 4 * 256;
    println!(
        "SEG_DATA_BYTES={} bytes/record={} one_segment_ceiling={} writing={}",
        slate_kv_core::config::SEG_DATA_BYTES,
        per_record,
        slate_kv_core::config::SEG_DATA_BYTES / per_record,
        n
    );

    {
        let db = Db::open(&dir, KeySource::Bytes([0x42; 32]), opts.clone()).expect("open");
        for i in 0..n {
            let k = format!("k{i:04}");
            db.put(k.as_bytes(), &val_for(i)).expect("put");
            db.commit().expect("commit");
        }
        let mut live = 0;
        for i in 0..n {
            let k = format!("k{i:04}");
            if let Ok(Some(v)) = db.get(k.as_bytes()) {
                if v == val_for(i) {
                    live += 1;
                }
            }
        }
        println!("before remount: {live}/{n} readable");
    }

    let db = Db::open(&dir, KeySource::Bytes([0x42; 32]), opts).expect("reopen");
    let mut ok = 0;
    let mut first_missing = None;
    for i in 0..n {
        let k = format!("k{i:04}");
        match db.get(k.as_bytes()) {
            Ok(Some(v)) if v == val_for(i) => ok += 1,
            _ => {
                if first_missing.is_none() {
                    first_missing = Some(i)
                }
            }
        }
    }
    println!("after  remount: {ok}/{n} readable, first_missing={first_missing:?}");
    println!(
        "verdict: {}",
        if ok == n {
            "REAL ENGINE OK -- multi-segment replay works; sim_db harness is the stale one"
        } else {
            "REAL ENGINE ALSO CAPS -- this is an engine defect, not a harness artifact"
        }
    );

    let _ = std::fs::remove_dir_all(&dir);
}
