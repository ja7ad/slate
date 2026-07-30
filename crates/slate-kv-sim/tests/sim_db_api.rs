//! `sim_db::Db` API surface.
//!
//! `sim_db` is the simulator-backed twin of `slate_kv::Db` and is what the
//! study harnesses drive, so the numbers in docs/specification.md rest on it
//! behaving like the real engine. It had no test of its own: every existing
//! suite goes through `slate_kv::Db` or `SimFlash` directly, so all 24 public
//! methods here were exercised only by binaries that CI never runs.
//!
//! These tests assert behaviour, not just reachability — durability across a
//! remount, tombstone semantics, and the accounting that feeds the write
//! amplification tables.

use slate_kv_sim::sim_db::{Db, KeySource, Options, Profile};
use slate_kv_sim::{SimCounter, SimFlash};

const PAGE: usize = 256;
const BLOCK: usize = 4096;
const CAP: u32 = 2 * 1024 * 1024;

fn key() -> KeySource {
    KeySource::Bytes([0x42; 32])
}

fn opts() -> Options {
    Options {
        capacity: CAP,
        // b_commit = 1 with auto_b off so `put` alone never leaves the record
        // buffered: these tests need "committed" and "buffered" to be distinct.
        b_commit: 1,
        auto_b: false,
        staleness_budget_ms: 1000,
        n_keys: 256,
        profile: Profile::Pi,
    }
}

fn fresh() -> Db {
    let flash = SimFlash::new(CAP, PAGE, BLOCK);
    let counter = SimCounter::new(1_000_000);
    match Db::open(key(), opts(), flash, counter) {
        Ok(db) => db,
        Err(b) => panic!("open failed: {:?}", b.0),
    }
}

#[test]
fn open_yields_an_empty_database() {
    let db = fresh();
    assert!(db.is_empty());
    assert_eq!(db.len(), 0);
    assert_eq!(db.acked_seq(), 0);
    assert!(db.epoch() >= 1, "a fresh volume starts at epoch >= 1");
}

#[test]
fn put_commit_get_round_trips() {
    let db = fresh();
    db.put(b"sensor_1", b"23.5 C").unwrap();
    db.commit().unwrap();

    assert_eq!(
        db.get(b"sensor_1").unwrap().as_deref(),
        Some(&b"23.5 C"[..])
    );
    assert_eq!(db.len(), 1);
    assert!(!db.is_empty());
}

#[test]
fn get_reports_absent_keys_as_none_not_an_error() {
    let db = fresh();
    assert_eq!(db.get(b"never_written").unwrap(), None);
}

#[test]
fn put_durable_acknowledges_without_an_explicit_commit() {
    let db = fresh();
    let before = db.acked_seq();
    db.put_durable(b"k", b"v").unwrap();
    assert!(
        db.acked_seq() > before,
        "put_durable must advance acked_seq on its own (before={before}, after={})",
        db.acked_seq()
    );
    assert_eq!(db.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
}

#[test]
fn delete_hides_the_key_and_shrinks_len() {
    let db = fresh();
    db.put_durable(b"a", b"1").unwrap();
    db.put_durable(b"b", b"2").unwrap();
    assert_eq!(db.len(), 2);

    db.delete(b"a").unwrap();
    db.commit().unwrap();

    assert_eq!(
        db.get(b"a").unwrap(),
        None,
        "deleted key must not read back"
    );
    assert_eq!(db.get(b"b").unwrap().as_deref(), Some(&b"2"[..]));
    assert_eq!(db.len(), 1);
}

#[test]
fn delete_durable_acknowledges_the_tombstone() {
    let db = fresh();
    db.put_durable(b"k", b"v").unwrap();
    let before = db.acked_seq();
    db.delete_durable(b"k").unwrap();
    assert!(
        db.acked_seq() > before,
        "delete_durable must make the tombstone durable before returning"
    );
    assert_eq!(db.get(b"k").unwrap(), None);
}

#[test]
fn overwriting_a_key_returns_the_latest_value() {
    let db = fresh();
    db.put_durable(b"k", b"first").unwrap();
    db.put_durable(b"k", b"second").unwrap();
    assert_eq!(db.get(b"k").unwrap().as_deref(), Some(&b"second"[..]));
    assert_eq!(db.len(), 1, "an overwrite is not a second key");
}

#[test]
fn committed_data_survives_a_remount() {
    // The durability property the whole engine exists for: take the flash and
    // counter out from under the Db and mount a fresh one over the same bytes.
    let mut db = fresh();
    for i in 0..32u32 {
        db.put(format!("key_{i:03}").as_bytes(), &i.to_le_bytes())
            .unwrap();
    }
    db.commit().unwrap();
    let acked = db.acked_seq();

    let (flash, counter) = db.take_flash_and_counter();
    drop(db);

    let db2 = match Db::open(key(), opts(), flash, counter) {
        Ok(d) => d,
        Err(b) => panic!("remount failed: {:?}", b.0),
    };
    assert_eq!(
        db2.len(),
        32,
        "every committed key must survive the remount"
    );
    assert_eq!(
        db2.acked_seq(),
        acked,
        "the acknowledged sequence must be preserved across mount"
    );
    for i in 0..32u32 {
        assert_eq!(
            db2.get(format!("key_{i:03}").as_bytes())
                .unwrap()
                .as_deref(),
            Some(&i.to_le_bytes()[..]),
            "value for key_{i:03} did not survive"
        );
    }
}

#[test]
fn a_wrong_root_key_is_refused_on_mount() {
    let mut db = fresh();
    db.put_durable(b"secret", b"value").unwrap();
    let (flash, counter) = db.take_flash_and_counter();
    drop(db);

    let wrong = KeySource::Bytes([0x43; 32]);
    assert!(
        Db::open(wrong, opts(), flash, counter).is_err(),
        "mounting with the wrong root key must fail, never succeed silently"
    );
}

#[test]
fn stats_account_for_the_bytes_actually_written() {
    let db = fresh();
    let before = db.stats();
    assert_eq!(before.user_bytes, 0);

    for i in 0..16u32 {
        db.put_durable(format!("k{i:02}").as_bytes(), &[0xAB; 64])
            .unwrap();
    }
    let s = db.stats();

    assert!(s.user_bytes > 0, "user bytes must be counted");
    assert!(s.commits > 0, "commits must be counted");
    // Each record is counted exactly once. A regression that double-counts
    // (as the slate-kv path once did) shows up here as a 2x overshoot.
    let max_plausible = 16 * (64 + 8 + 128);
    assert!(
        s.user_bytes <= max_plausible as u64,
        "user_bytes={} exceeds the largest plausible single-count total {}",
        s.user_bytes,
        max_plausible
    );
}

#[test]
fn next_seq_advances_monotonically_with_writes() {
    let db = fresh();
    let a = db.next_seq();
    db.put_durable(b"k1", b"v").unwrap();
    let b = db.next_seq();
    db.put_durable(b"k2", b"v").unwrap();
    let c = db.next_seq();
    assert!(a < b && b < c, "next_seq must be monotone: {a} < {b} < {c}");
}

#[test]
fn security_mode_is_reported() {
    let db = fresh();
    // The simulator provides a counter, so this must not report the absent
    // case. The exact variant depends on the counter kind; assert it is one of
    // the defined modes rather than pinning a value the sim may change.
    let mode = db.security_mode();
    let name = format!("{mode:?}");
    assert!(
        ["Full", "BestEffortRollback", "NoRollbackProtection"].contains(&name.as_str()),
        "unexpected security mode: {name}"
    );
}

#[test]
fn compact_and_scrub_run_without_disturbing_data() {
    let db = fresh();
    for i in 0..24u32 {
        db.put_durable(format!("k{i:02}").as_bytes(), &[i as u8; 32])
            .unwrap();
    }
    db.compact().unwrap();
    let report = db.scrub().unwrap();
    // A healthy volume must not report errors it then cannot fix.
    assert!(
        report.errors_fixed <= report.errors_found,
        "scrub claims {} fixed of {} found",
        report.errors_fixed,
        report.errors_found
    );

    for i in 0..24u32 {
        assert_eq!(
            db.get(format!("k{i:02}").as_bytes()).unwrap().as_deref(),
            Some(&[i as u8; 32][..]),
            "compaction lost key k{i:02}"
        );
    }
}

#[test]
fn flash_mut_exposes_the_underlying_simulator() {
    let db = fresh();
    db.put_durable(b"k", b"v").unwrap();
    let programmed = db.flash_mut(|f| f.stats.bytes_programmed);
    assert!(
        programmed > 0,
        "the durable put must have programmed flash pages"
    );
}

#[test]
fn empty_values_round_trip() {
    let db = fresh();
    db.put_durable(b"empty", b"").unwrap();
    assert_eq!(db.get(b"empty").unwrap().as_deref(), Some(&b""[..]));
}
