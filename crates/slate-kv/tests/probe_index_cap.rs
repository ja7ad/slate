//! Probe: does Options::n_keys actually govern how many keys fit? db.rs sizes
//! the index arena from n_keys via floating point, then passes index_len/4 as
//! n_buckets, independent of the compile-time N_BUCKETS = 2048.

use slate_kv::{Db, KeySource, Options};

fn try_n_keys(n_keys: usize) -> (usize, Option<String>) {
    let dir =
        std::env::temp_dir().join(format!("slate_probe_idx_{}_{}", std::process::id(), n_keys));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let opts = Options {
        capacity: 16 * 1024 * 1024,
        n_keys,
        ..Default::default()
    };
    let db = Db::open(&dir, KeySource::Bytes([13u8; 32]), opts).unwrap();

    let mut inserted = 0usize;
    let mut err = None;
    // Try to insert 4x the requested capacity of DISTINCT keys.
    for i in 0..(n_keys * 4) {
        let k = format!("distinct_key_{i:08}");
        match db.put(k.as_bytes(), b"v") {
            Ok(()) => inserted += 1,
            Err(e) => {
                err = Some(format!("{e:?}"));
                break;
            }
        }
    }
    let reported = db.len();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    println!("PROBE n_keys={n_keys} inserted={inserted} db_len={reported} err={err:?}");
    (inserted, err)
}

#[test]
fn probe_index_capacity_follows_n_keys() {
    let (small, _) = try_n_keys(2048);
    let (large, _) = try_n_keys(8192);
    println!("PROBE small={small} large={large}");
    assert!(
        large > small,
        "raising n_keys from 2048 to 8192 did not raise usable key capacity \
         ({small} vs {large})"
    );
}
