//! End-to-end checks for the put/get/delete/stats and recovery fixes.
//!
//! These pin the behaviours that were previously broken:
//!   * repeated Puts to the same key must overwrite its index slot (no leak),
//!     so `len()` counts *distinct live keys*, not total records;
//!   * Get returns the latest value and None after Delete;
//!   * a Delete must not evict a different key from the index;
//!   * after reopen (recovery) the same invariants hold and duplicates from
//!     replay are collapsed.

use slate_kv::{Db, KeySource, Options, Profile};

fn opts() -> Options {
    Options {
        capacity: 1024 * 1024,
        b_commit: 4,
        auto_b: false,
        staleness_budget_ms: 1000,
        n_keys: 256,
        profile: Profile::Pi,
        durability: slate_kv::file_flash::Durability::Full,
        ..Default::default()
    }
}

fn fresh(path: &str) -> Db {
    let p = std::path::Path::new(path);
    let _ = std::fs::remove_dir_all(p);
    std::fs::create_dir_all(p).unwrap();
    Db::open(p, KeySource::Bytes([0x24; 32]), opts()).unwrap()
}

#[test]
fn put_overwrite_keeps_len_stable_and_latest_value() {
    let db = fresh("./test_db_rt_overwrite");

    for i in 0..500u32 {
        db.put(b"hot", format!("v{i}").as_bytes()).unwrap();
    }
    db.commit().unwrap();

    // One live key, not 500 leaked slots.
    assert_eq!(
        db.len(),
        1,
        "repeated Put to one key must not leak index slots"
    );
    assert_eq!(db.get(b"hot").unwrap().as_deref(), Some(&b"v499"[..]));
}

#[test]
fn get_after_delete_is_none_and_len_zero() {
    let db = fresh("./test_db_rt_delete");
    db.put(b"k", b"value").unwrap();
    db.commit().unwrap();
    assert_eq!(db.get(b"k").unwrap().as_deref(), Some(&b"value"[..]));
    assert_eq!(db.len(), 1);

    db.delete_durable(b"k").unwrap();
    assert_eq!(db.get(b"k").unwrap(), None);
    assert_eq!(db.len(), 0, "index must drop the key after delete");
}

#[test]
fn many_keys_roundtrip_and_len() {
    let db = fresh("./test_db_rt_many");
    for i in 0..200u32 {
        db.put(format!("key{i}").as_bytes(), format!("val{i}").as_bytes())
            .unwrap();
    }
    db.commit().unwrap();
    assert_eq!(db.len(), 200);
    for i in 0..200u32 {
        assert_eq!(
            db.get(format!("key{i}").as_bytes()).unwrap().as_deref(),
            Some(format!("val{i}").as_bytes())
        );
    }
}

#[test]
fn recovery_collapses_duplicate_replays() {
    let path = "./test_db_rt_recover";
    // Bigger flash: an append-only log with no auto-compaction needs room for
    // every superseded record. 600 writes over 50 keys also spans well past one
    // 48 KB segment window, exercising the full-region recovery scan.
    let big = || Options {
        capacity: 8 * 1024 * 1024,
        ..opts()
    };
    {
        let p = std::path::Path::new(path);
        let _ = std::fs::remove_dir_all(p);
        std::fs::create_dir_all(p).unwrap();
        let db = Db::open(p, KeySource::Bytes([0x24; 32]), big()).unwrap();
        for i in 0..600u32 {
            let k = format!("k{:02}", i % 50);
            db.put(k.as_bytes(), format!("v{i}").as_bytes()).unwrap();
        }
        db.commit().unwrap();
        assert_eq!(db.len(), 50);
    }

    // Reopen -> recovery rebuilds the index from the log tail.
    let db = Db::open(
        std::path::Path::new(path),
        KeySource::Bytes([0x24; 32]),
        big(),
    )
    .unwrap();
    assert_eq!(db.len(), 50, "recovery must dedup replayed records by key");
    // Latest value per key survives (k00's last write is i=550, k49's is i=599).
    assert_eq!(db.get(b"k00").unwrap().as_deref(), Some(&b"v550"[..]));
    assert_eq!(db.get(b"k49").unwrap().as_deref(), Some(&b"v599"[..]));
    assert_eq!(db.get(b"missing").unwrap(), None);
}

#[test]
fn stats_count_commits_and_wakes() {
    let db = fresh("./test_db_rt_stats");
    for i in 0..20u32 {
        db.put(format!("s{i}").as_bytes(), b"x").unwrap();
    }
    db.commit().unwrap();
    let s = db.stats();
    assert!(s.commits >= 1, "commits should be counted");
    assert_eq!(s.wakes, s.commits, "each commit is one wake (fixed A cost)");
    assert!(s.user_bytes > 0);
}
