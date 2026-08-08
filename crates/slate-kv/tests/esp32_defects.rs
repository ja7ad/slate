//! Regression tests for the three defects reported from the ESP32-C3 board
//! (kv_demo and embassy_demo, 2026-07-29).
//!
//! Each test fails against the pre-fix engine:
//!
//!   1. `get_before_commit_returns_value` — kv_demo's `get` read candidate
//!      offsets straight off flash, but `put` leaves the record in the
//!      uncommitted RAM batch, whose offset is a *future* flash address that
//!      still reads as erased. `put sensor_0 25` then `get sensor_0` printed
//!      `(not found)`.
//!
//!   2. `sustained_writes_survive_region_wraparound` — no production path ever
//!      moved a segment to `SegState::Sealed`, so `pick_victim` always returned
//!      `None`, compaction was a permanent no-op, and the log head ran past the
//!      last segment. On the board this bricked embassy_demo at ~8100 records
//!      with `EspFlash program error: not erased at addr 2096640` repeating
//!      forever.
//!
//!   3. `write_amplification_exceeds_one_after_compaction` — `add_gc_bytes`,
//!      `add_parity_bytes`, `add_ckpt_bytes` and `add_erase` had zero call
//!      sites anywhere in the workspace, so any reported write amplification was
//!      identically 1.0 for every workload and the claim was unfalsifiable.

use slate_kv::{Db, KeySource, Options, Profile};

fn opts(capacity: u32) -> Options {
    Options {
        capacity,
        b_commit: 4,
        auto_b: false,
        staleness_budget_ms: 1000,
        n_keys: 256,
        profile: Profile::Pi,
        durability: slate_kv::file_flash::Durability::Full,
    }
}

fn fresh(path: &str, capacity: u32) -> Db {
    let p = std::path::Path::new(path);
    let _ = std::fs::remove_dir_all(p);
    std::fs::create_dir_all(p).unwrap();
    Db::open(p, KeySource::Bytes([0x24; 32]), opts(capacity)).unwrap()
}

/// Options for tests whose subject is allocation rather than durability.
///
/// The durability barrier fires on every *page program*, not every commit, so
/// a few thousand records issue tens of thousands of barriers. At roughly 8 ms
/// each that dominates everything: a 20 000-op run measured 19m34s wall against
/// 0.6s of user CPU. `Durability::OsCache` does not help on macOS, where Rust's
/// `sync_data()` maps onto the same `F_FULLFSYNC`.
///
/// Dropping the barrier entirely is sound *for these tests specifically*: they
/// assert segment allocation, reclaim, reuse and index correctness, none of
/// which depend on whether a write reached stable media. Crash consistency is
/// what needs `Full`, and it is covered by the crash-injection suite in
/// `slate-kv-sim`, which simulates power loss directly rather than relying on
/// the host filesystem.
fn opts_fast(capacity: u32) -> Options {
    Options {
        durability: slate_kv::file_flash::Durability::None,
        ..opts(capacity)
    }
}

fn fresh_fast(path: &str, capacity: u32) -> Db {
    let p = std::path::Path::new(path);
    let _ = std::fs::remove_dir_all(p);
    std::fs::create_dir_all(p).unwrap();
    Db::open(p, KeySource::Bytes([0x24; 32]), opts_fast(capacity)).unwrap()
}

/// Defect 1: a value must be readable immediately after `put`, before any
/// commit, because the record is in the batch and the index points at where it
/// *will* land.
#[test]
fn get_before_commit_returns_value() {
    let db = fresh("./test_db_esp_getcommit", 1024 * 1024);

    db.put(b"sensor_0", b"25").unwrap();

    // No commit() here on purpose: this is the exact board sequence.
    let got = db.get(b"sensor_0").unwrap();
    assert_eq!(
        got.as_deref(),
        Some(&b"25"[..]),
        "get must resolve a record still in the uncommitted batch"
    );

    // And it must still read back after the batch is durable.
    db.commit().unwrap();
    assert_eq!(db.get(b"sensor_0").unwrap().as_deref(), Some(&b"25"[..]));

    // A deleted key reads back as absent, not as a stale value.
    db.delete(b"sensor_0").unwrap();
    assert_eq!(db.get(b"sensor_0").unwrap(), None);
}

/// Defect 2: exhausting the log must fail *diagnosably* and without losing
/// data, and compaction must actually run rather than silently no-op.
///
/// On the board the pre-fix engine reported `Io` forever once the head passed
/// the last segment (`EspFlash program error: not erased at addr 2096640`,
/// repeating), because nothing ever sealed a segment so `pick_victim` always
/// returned `None`. Now: exhaustion is `FlashFull`, every committed record is
/// still readable, and compaction is reached.
///
/// NOTE: this deliberately does *not* assert that the reclaimed space is
/// reused. See `space_reuse_after_reclaim` below — reuse needs a circular,
/// segment-aware log head, which the current on-flash format does not support.
#[test]
fn log_exhaustion_is_diagnosable_and_lossless() {
    // The region cannot usefully be made smaller: `data_base_offset` reserves
    // 528 KiB for checkpoint slots before the first log byte, so 1 MiB already
    // leaves only 10 segments and 768 KiB leaves 5. Cost is controlled through
    // the durability mode instead — what this test asserts is how exhaustion is
    // reported, not whether the write reached stable media.
    let capacity = 1024 * 1024;
    // Every key here is distinct, so the cuckoo index grows with the run. At
    // the default `n_keys` the index fills first and the engine reports
    // `IndexFull` — a correct diagnosis, but a different one from the exhaustion
    // this test is about. Size the index past what the region can hold so the
    // flash is what runs out.
    let db = Db::open(
        {
            let p = std::path::Path::new("./test_db_esp_wrap");
            let _ = std::fs::remove_dir_all(p);
            std::fs::create_dir_all(p).unwrap();
            p
        },
        KeySource::Bytes([0x24; 32]),
        Options {
            n_keys: 8192,
            ..opts_fast(capacity)
        },
    )
    .unwrap();

    let val = [b'v'; 64];
    let mut written = Vec::new();
    let mut err = None;

    for i in 0..8000u32 {
        let key = format!("key{i:05}");
        match db.put(key.as_bytes(), &val) {
            Ok(()) => written.push(key),
            Err(e) => {
                err = Some(e);
                break;
            }
        }
        if i % 256 == 0 {
            if let Err(e) = db.commit() {
                err = Some(e);
                break;
            }
        }
    }

    // Every key here is distinct, so nothing is ever garbage and reclaim cannot
    // free anything: the region genuinely fills. That is the post-fix meaning of
    // `FlashFull` — the live set does not fit — rather than the pre-fix meaning,
    // which was merely that the head had reached the end of the address space.
    let err = err.unwrap_or_else(|| {
        let st = db.stats();
        panic!(
            "expected exhaustion, but all {} distinct records were accepted \
             into a log area of {} segments. Before the futile-reclaim guard \
             the engine answered this workload by relocating live data \
             forever rather than reporting FlashFull: {st:?}",
            written.len(),
            st.segments
        )
    });
    let msg = format!("{err:?}");
    assert!(
        msg.contains("FlashFull"),
        "exhaustion must name itself as FlashFull, not surface as an opaque Io: got {msg}"
    );

    // Nothing already acknowledged may be lost. Check a sample across the run.
    let _ = db.commit();
    let step = (written.len() / 50).max(1);
    for key in written.iter().step_by(step) {
        assert_eq!(
            db.get(key.as_bytes()).unwrap().as_deref(),
            Some(&val[..]),
            "record {key} was lost before the region filled"
        );
    }

    let st = db.stats();
    assert_eq!(
        st.gc_open_failed, 0,
        "compaction failed to decrypt records it then treated as garbage: {st:?}"
    );
    assert!(
        st.ckpt_bytes > 0,
        "no checkpoint was ever written, so GC could never qualify a victim: {st:?}"
    );
}

/// Approaching exhaustion must not thrash epoch seals.
///
/// An epoch seal costs a full checkpoint (33 KiB and 9 erases at default
/// geometry). `reserve_space_async` seals when victim selection comes up empty,
/// but with zero `Sealed` segments a seal *cannot* produce a victim — so an
/// unguarded seal there fires on every commit once space runs low. Observed on
/// the C3 before the guard: 13 checkpoints in the last 112 records, epoch racing
/// 8 -> 25, and write amplification climbing 2.7435 -> 3.6163 while reclaiming
/// nothing.
#[test]
fn no_epoch_seal_thrash_near_exhaustion() {
    let db = fresh_fast("./test_db_esp_thrash", 2 * 1024 * 1024);
    let val = [b'v'; 16];

    let mut epochs = Vec::new();
    for i in 0..4000u32 {
        if db.put(b"async_test_key", &val).is_err() {
            break;
        }
        if i % 1000 == 0 {
            epochs.push(db.epoch());
        }
    }
    let _ = db.commit();

    let st = db.stats();
    let epoch = db.epoch();

    // Reclaim must actually have happened — otherwise this test would pass
    // trivially on an engine where GC never runs.
    assert!(
        st.gc_segments_freed > 0,
        "no segment was reclaimed, so this test is not exercising the guard: {st:?}"
    );

    // A seal per ~1000 records is the Θ/deadline-driven norm here; dozens would
    // mean the reserve path is sealing on every commit.
    assert!(
        epoch < 15,
        "epoch reached {epoch} — epoch seals are thrashing (checkpoints={} B, epochs seen {:?})",
        st.ckpt_bytes,
        epochs
    );
}

/// Space freed by reclaim must be reusable by the append head.
///
/// This is the property every ESP32-C3 run failed and that all five IoT-J
/// reviewers raised as blocking: the engine halted with `FlashFull` while 29 of
/// 31 segments were free and erased. Garbage collection worked; the append head
/// simply had no way to move back into the space it freed, so the device's
/// usable lifetime was one linear pass of the region.
///
/// Two format-level causes had to be closed before this could pass:
/// `SEG_BYTES` is 12 erase blocks (49 152 B) of which only the first 8 are data
/// (32 768 B), and the writer ran the head straight through both the parity
/// blocks and the next segment boundary — so records straddled segments and a
/// reclaimed segment's first byte was mid-ciphertext. And nothing wrote the
/// on-flash segment headers that let recovery order a wrapped log by `seg_seq`
/// rather than by address (doc 002 §2.1-2.2).
#[test]
fn space_reuse_after_reclaim() {
    let capacity = 1024 * 1024;
    let db = fresh_fast("./test_db_esp_reuse", capacity);
    let val = [b'v'; 64];

    // Overwrite a tiny key set: the live set stays ~1 KiB while total traffic
    // far exceeds the region, so only reuse of reclaimed space can sustain it.
    // Enough to cross a 1 MiB region several times over. The exhaustive
    // hundred-pass version runs against the in-memory device in
    // `slate-kv-sim/tests/log_wrap.rs`; what this adds is that reuse also works
    // through a real file, which is worth a few thousand commits and no more.
    for i in 0..4000u32 {
        let key = format!("key{:05}", i % 16);
        db.put(key.as_bytes(), &val)
            .unwrap_or_else(|e| panic!("put #{i} failed: {e:?} — reclaimed space not reused"));
        if i % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    assert_eq!(db.get(b"key00000").unwrap().as_deref(), Some(&val[..]));
}

/// The long multi-cycle wrap tests live in `slate-kv-sim`, not here.
///
/// `wrap_survives_many_capacity_cycles` and its companions drive hundreds of
/// thousands of commits. Through `FileFlash` each of those commits pays a
/// durability barrier — ~8 ms on macOS, where `Durability::Full` and
/// `OsCache` are indistinguishable because Rust's `sync_data()` maps onto the
/// same `F_FULLFSYNC`, and far worse on a Raspberry Pi's SD card, where one
/// such run held a core for over 24 minutes without finishing. What those
/// tests assert — segment allocation, reclaim, reuse, index correctness — does
/// not involve the filesystem at all, so they run against the in-memory
/// `SimFlash` in `slate-kv-sim/tests/log_wrap.rs` and complete in well under a
/// second.
///
/// What stays here is the part that genuinely needs a real file: that a
/// wrapped log survives being closed and reopened from bytes on disk.

/// Data written before a wrap must survive a remount.
///
/// Recovery replays forward from the checkpoint head to the first erased page.
/// Once the head can wrap to a lower address, "forward" is no longer the same
/// as "ascending address", so a wrapped log is only replayable if recovery
/// orders segments by allocation number. This test fails against an
/// address-ordered recovery even when the allocator itself is correct.
#[test]
fn wrapped_log_survives_remount() {
    let path = "./test_db_esp_wrap_remount";
    let capacity = 1024 * 1024;
    let val = [b'v'; 64];
    const KEYS: u32 = 16;

    {
        let db = fresh_fast(path, capacity);
        // Enough traffic to wrap the region several times over.
        for i in 0..3_000u32 {
            let key = format!("key{:05}", i % KEYS);
            db.put(key.as_bytes(), &val).unwrap();
            if i % 256 == 0 {
                db.commit().unwrap();
            }
        }
        db.commit().unwrap();
    }

    // Reopen from flash alone: index rebuilt from checkpoint plus tail replay.
    let db = Db::open(
        std::path::Path::new(path),
        KeySource::Bytes([0x24; 32]),
        opts_fast(capacity),
    )
    .expect("a wrapped log must remount");

    for k in 0..KEYS {
        let key = format!("key{k:05}");
        assert_eq!(
            db.get(key.as_bytes()).unwrap().as_deref(),
            Some(&val[..]),
            "key {key} did not survive a remount after the log wrapped"
        );
    }
}

/// Defect 3: write amplification must be measurable and strictly greater than
/// 1.0 once the engine has written parity pages, commit markers and a
/// checkpoint. Before instrumentation this was identically 1.0.
#[test]
fn write_amplification_exceeds_one_after_compaction() {
    let db = fresh("./test_db_esp_wa", 4 * 1024 * 1024);

    // Before any write, WA is unmeasured — not 1.0.
    assert_eq!(
        db.stats().write_amplification(),
        None,
        "an unmeasured workload must not report a WA number"
    );

    let val = [b'x'; 100];
    for i in 0..3000u32 {
        let key = format!("k{:05}", i % 32);
        db.put(key.as_bytes(), &val).unwrap();
        if i % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    db.compact().unwrap();

    let st = db.stats();
    let wa = st
        .write_amplification()
        .expect("user bytes were written, so WA must be measurable");

    assert!(
        st.parity_bytes > 0,
        "parity pages are programmed on every commit but were not counted: {st:?}"
    );
    assert!(
        st.marker_bytes > 0,
        "two commit-marker pages are written per commit but were not counted: {st:?}"
    );
    assert!(
        wa > 1.0,
        "write amplification must exceed 1.0 once overhead is counted, got {wa} ({st:?})"
    );
    // Sanity ceiling: a plausible engine, not a runaway counter.
    assert!(wa < 100.0, "implausible write amplification {wa} ({st:?})");
}
