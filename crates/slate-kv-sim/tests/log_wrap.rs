//! Circular-log acceptance tests: reclaimed space must be reusable.
//!
//! This is the defect all five IEEE IoT-J reviewers raised as blocking. Every
//! ESP32-C3 run halted with `FlashFull` while 29 of its 31 segments were free
//! and erased: garbage collection worked, but the append head could not move
//! back into the space it had freed, so a device's usable lifetime was one
//! linear pass of the flash region.
//!
//! These run against the in-memory `SimFlash` rather than `slate_kv`'s
//! file-backed flash. That is a deliberate choice about what is being measured:
//! the properties under test are segment allocation, reclaim, reuse and index
//! correctness, none of which involve the host filesystem. Going through
//! `FileFlash` would add a durability barrier per commit — on macOS both
//! `Durability::Full` and `Durability::OsCache` cost roughly 8 ms, since Rust's
//! `sync_data()` maps onto the same full barrier as `F_FULLFSYNC` — which puts
//! a multi-wrap run into the tens of minutes while exercising nothing these
//! tests assert. Crash consistency, which does need a real barrier, is covered
//! by the crash-injection suite.

use slate_kv_sim::sim_db::{Db, KeySource, Options, Profile};
use slate_kv_sim::{SimCounter, SimFlash};

const PAGE: usize = 256;
const BLOCK: usize = 4096;

/// A segment is 8 data blocks plus 4 parity blocks; reclaim erases all twelve,
/// so erases divided by this is the number of segments reclaimed.
const SEG_BLOCKS: u64 = 12;

fn fresh(capacity: u32) -> Db {
    let flash = SimFlash::new(capacity, PAGE, BLOCK);
    let counter = SimCounter::new(1_000_000);
    let opts = Options {
        capacity,
        b_commit: 4,
        auto_b: false,
        staleness_budget_ms: 1000,
        n_keys: 256,
        profile: Profile::Pi,
    };
    match Db::open(KeySource::Bytes([0x24; 32]), opts, flash, counter) {
        Ok(db) => db,
        Err(b) => panic!("open failed: {:?}", b.0),
    }
}

/// Space freed by reclaim must be reusable by the append head.
///
/// The live set stays tiny — a handful of keys, rewritten over and over — while
/// total write traffic far exceeds the region. Every sealed segment is
/// therefore fully reclaimable, and the only way to complete the run is to
/// allocate into space that has already been reclaimed at least once.
#[test]
fn space_reuse_after_reclaim() {
    let capacity = 1024 * 1024;
    let db = fresh(capacity);
    let val = [b'v'; 64];

    for i in 0..20_000u32 {
        let key = format!("key{:05}", i % 16);
        db.put(key.as_bytes(), &val)
            .unwrap_or_else(|e| panic!("put #{i} failed: {e:?} — reclaimed space not reused"));
        if i % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();

    assert_eq!(db.get(b"key00000").unwrap().as_deref(), Some(&val[..]));

    let st = db.stats();
    assert!(
        st.erases > 0,
        "a run this long must have reclaimed segments: {st:?}"
    );
    assert_eq!(
        st.gc_open_failed, 0,
        "compaction failed to decrypt records — silent data loss: {st:?}"
    );
}

/// Sustained operation across many complete fill/reclaim cycles.
///
/// Reviewers 2 and 5 both asked for evidence beyond a single pass: a wrap that
/// happens once, into one free segment, and then stops would satisfy the test
/// above while still being useless for a long-lived unattended deployment.
/// Correctness is checked mid-flight as well as at the end, so a wrap that
/// silently drops a key cannot hide behind a final-state assertion.
#[test]
fn wrap_survives_many_capacity_cycles() {
    let capacity = 1024 * 1024;
    let db = fresh(capacity);
    let val = [b'v'; 64];
    const KEYS: u32 = 16;
    const OPS: u32 = 200_000;

    for i in 0..OPS {
        let key = format!("key{:05}", i % KEYS);
        db.put(key.as_bytes(), &val)
            .unwrap_or_else(|e| panic!("put #{i} of {OPS} failed: {e:?}"));

        if i % 256 == 0 {
            db.commit().unwrap();
        }

        if i % 25_000 == 0 && i > 0 {
            db.commit().unwrap();
            for k in 0..KEYS {
                let probe = format!("key{k:05}");
                assert_eq!(
                    db.get(probe.as_bytes()).unwrap().as_deref(),
                    Some(&val[..]),
                    "key {probe} was lost or corrupted after {i} writes"
                );
            }
        }
    }
    db.commit().unwrap();

    for k in 0..KEYS {
        let key = format!("key{k:05}");
        assert_eq!(
            db.get(key.as_bytes()).unwrap().as_deref(),
            Some(&val[..]),
            "key {key} did not survive {OPS} writes across the region"
        );
    }

    // Count region passes from erases actually performed rather than from a
    // byte estimate: each commit also writes a parity page and two marker
    // pages, so physical traffic per record is several times the record size.
    let st = db.stats();
    let segments_reclaimed = st.erases / SEG_BLOCKS;
    let passes = segments_reclaimed / st.segments.max(1) as u64;
    assert!(
        passes >= 100,
        "expected at least 100 complete region passes, got {passes} \
         ({segments_reclaimed} segments reclaimed across {} segments): {st:?}",
        st.segments
    );
    assert_eq!(
        st.gc_open_failed, 0,
        "compaction failed to decrypt records — silent data loss: {st:?}"
    );
}

/// Write amplification must stay bounded once the log is wrapping.
///
/// A circular allocator that thrashed — reclaiming a segment per commit, or
/// re-checkpointing to qualify each victim — would pass the reuse tests while
/// destroying flash endurance. The pre-fix engine did exactly this near
/// exhaustion: 13 checkpoints in 112 records, pushing WA from 2.74 to 3.62.
#[test]
fn wrapping_does_not_thrash_write_amplification() {
    let capacity = 1024 * 1024;
    let db = fresh(capacity);
    let val = [b'v'; 64];

    for i in 0..50_000u32 {
        let key = format!("key{:05}", i % 16);
        db.put(key.as_bytes(), &val).unwrap();
        if i % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();

    let st = db.stats();
    let wa = st
        .write_amplification()
        .expect("user bytes were written, so WA must be measurable");

    // At b_commit = 4 with 74-byte records the floor is dominated by the parity
    // page and two marker pages per commit; the ceiling here is generous and
    // exists to catch thrash, not to pin a specific value.
    assert!(
        (1.0..8.0).contains(&wa),
        "write amplification {wa} is outside the plausible band for a wrapping \
         log — check for reclaim or checkpoint thrash: {st:?}"
    );

    // Checkpoints must not be written per-commit. One per reclaimed segment is
    // the expected order of magnitude.
    let segments_reclaimed = st.erases / SEG_BLOCKS;
    assert!(
        st.ckpt_bytes < st.user_bytes * 4,
        "checkpoint traffic ({} B) dwarfs user data ({} B) after \
         {segments_reclaimed} reclaims — epoch-seal thrash: {st:?}",
        st.ckpt_bytes,
        st.user_bytes
    );
}
