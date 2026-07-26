//! End-to-end tests for the epoch lifecycle.
//!
//! These pin the behaviour that the Θ-triggered epoch seal actually fires under
//! ordinary writes. Before the counter was wired up, `records_in_epoch` stayed
//! at zero forever, so no checkpoint was ever written: mount replayed the whole
//! log (unbounded, not the O(Θ) the design claims), GC could never select a
//! victim, and record keys never rotated. All three failures were invisible
//! from the public API — the database returned correct values throughout — so
//! they need explicit tests rather than being caught by a roundtrip check.

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

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "slate_epoch_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Writing more than Θ records must advance the epoch.
#[test]
fn epoch_advances_after_theta_records() {
    let dir = tmpdir("advance");
    let db = Db::open(&dir, KeySource::Bytes([7u8; 32]), opts(&dir)).unwrap();

    let start = db.epoch();
    // Θ + slack, so the trigger is crossed even though a commit only checks the
    // counter at batch boundaries.
    for i in 0..(THETA + 512) {
        let k = format!("k{i:06}");
        db.put(k.as_bytes(), b"v").unwrap();
    }
    let after = db.epoch();

    assert!(
        after > start,
        "epoch did not advance after {} records (start={start}, after={after}) — \
         the Theta trigger never fired",
        THETA + 512
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Values written before an epoch roll must still read back after it. This is
/// the property that per-epoch record keys put at risk: the log is not
/// rewritten on a roll, so old records must be opened with the key of the epoch
/// they were sealed under.
#[test]
fn values_survive_epoch_roll() {
    let dir = tmpdir("survive");
    let db = Db::open(&dir, KeySource::Bytes([9u8; 32]), opts(&dir)).unwrap();

    let start = db.epoch();
    db.put(b"early_key", b"early_value").unwrap();

    for i in 0..(THETA + 512) {
        let k = format!("filler{i:06}");
        db.put(k.as_bytes(), b"x").unwrap();
    }
    assert!(
        db.epoch() > start,
        "test precondition: epoch must have rolled"
    );

    let got = db.get(b"early_key").unwrap();
    assert_eq!(
        got.as_deref(),
        Some(&b"early_value"[..]),
        "a record sealed before the epoch roll became unreadable after it"
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A clean close must not lose the pending batch. Records live in the batch
/// buffer until a commit marker is written, so dropping the handle after a
/// successful `put` used to discard up to `b_max` of them with no error.
#[test]
fn clean_close_preserves_uncommitted_batch() {
    let dir = tmpdir("close");
    let o = opts(&dir);
    let db = Db::open(&dir, KeySource::Bytes([13u8; 32]), o.clone()).unwrap();
    // Fewer than one commit batch, so these are still pending at drop.
    for i in 0..3 {
        db.put(format!("late{i}").as_bytes(), b"v").unwrap();
    }
    drop(db);

    let db2 = Db::open(&dir, KeySource::Bytes([13u8; 32]), o).unwrap();
    for i in 0..3 {
        assert_eq!(
            db2.get(format!("late{i}").as_bytes()).unwrap().as_deref(),
            Some(&b"v"[..]),
            "record {i} written before a clean close was lost"
        );
    }
    drop(db2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// After a roll, a reopen must load the checkpoint and see all data. This
/// exercises the mount path against a volume that actually contains a
/// checkpoint — previously unreachable, since none was ever written.
#[test]
fn reopen_after_epoch_roll_preserves_data() {
    let dir = tmpdir("reopen");
    let db = Db::open(&dir, KeySource::Bytes([11u8; 32]), opts(&dir)).unwrap();

    db.put(b"persisted", b"through_reopen").unwrap();
    for i in 0..(THETA + 512) {
        let k = format!("f{i:06}");
        db.put(k.as_bytes(), b"y").unwrap();
    }
    let epoch_before = db.epoch();
    let len_before = db.len();
    drop(db);

    let db2 = Db::open(&dir, KeySource::Bytes([11u8; 32]), opts(&dir))
        .expect("reopen after an epoch roll must succeed");
    assert_eq!(
        db2.get(b"persisted").unwrap().as_deref(),
        Some(&b"through_reopen"[..])
    );
    assert_eq!(db2.len(), len_before, "key count changed across reopen");
    assert!(
        db2.epoch() >= epoch_before,
        "epoch regressed across reopen: {} < {epoch_before}",
        db2.epoch()
    );
    drop(db2);
    let _ = std::fs::remove_dir_all(&dir);
}
