//! Probe: does the epoch ever advance during normal operation?
//! THETA = 16384 records should trigger seal_epoch per the design.

use slate_kv::{Db, KeySource, Options};

fn opts() -> Options {
    Options {
        capacity: 4 * 1024 * 1024,
        n_keys: 4096,
        ..Default::default()
    }
}

#[test]
fn probe_epoch_advances_after_theta_records() {
    let dir = std::env::temp_dir().join(format!("slate_probe_epoch_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let db = Db::open(&dir, KeySource::Bytes([7u8; 32]), opts()).unwrap();
    let epoch_start = db.epoch();

    // Write well past THETA (16384) records.
    let theta = 16384usize;
    let mut wrote = 0usize;
    let mut first_err = None;
    for i in 0..(theta + 2000) {
        // Reuse a small key space so the index does not fill.
        let key = format!("k{:04}", i % 2000);
        match db.put(key.as_bytes(), b"v") {
            Ok(()) => wrote += 1,
            Err(e) => {
                first_err = Some(format!("{e:?} at i={i}"));
                break;
            }
        }
    }
    db.commit().ok();

    let epoch_end = db.epoch();
    println!("PROBE epoch_start={epoch_start} epoch_end={epoch_end} wrote={wrote} theta={theta}");
    println!(
        "PROBE next_seq={} acked_seq={}",
        db.next_seq(),
        db.acked_seq()
    );
    println!("PROBE first_err={first_err:?}");

    let _ = std::fs::remove_dir_all(&dir);

    // The design says an epoch seal (and thus a checkpoint) must occur every
    // THETA records. Assert it to see whether reality matches.
    assert!(
        epoch_end > epoch_start,
        "epoch never advanced after {wrote} records (start={epoch_start}, end={epoch_end})"
    );
}
