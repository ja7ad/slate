//! Paper measurement: mount (recovery) cost.
//!
//! The claim under test is that mount is an O(1) freshness-tip check plus an
//! O(Θ) replay of the tail written since the last checkpoint — so mount cost
//! must scale with *tail length* and be independent of *total volume written*.
//! Two sweeps, emitted into one CSV so they cannot disagree:
//!
//! * `experiment=tail_sweep` — total volume held fixed, replay tail swept over
//!   more than a decade (0 … 8000 records).
//! * `experiment=volume_sweep` — replay tail held fixed, total records written
//!   before the checkpoint swept over more than a decade. This is the sweep that
//!   actually tests the claim: if checkpointing did not work, mount time here
//!   would grow with the volume.
//!
//! Each row reports both the swept variable and the *realized* state it is
//! supposed to control — `records_replayed`, `replay_from`, `scan_bytes`,
//! `log_bytes_total` — plus `tail_intact`, which is false if an unplanned epoch
//! seal moved the checkpoint and shortened the tail. A row whose realized tail
//! does not match the requested one is not a measurement of the requested
//! operating point, and saying so in the data is the only way a reader can tell.
//!
//! Wall clock is reported as the median, min and max over `REPEATS` reopens of
//! the same on-disk volume. The spread matters: the first reopen reads through a
//! cold OS page cache and the rest do not, so a median alone would understate
//! the cost on a device that has just booted.
//!
//! `cargo run --release -p slate-kv --example paper_recovery`
//!
//! Platform: this measures `FileFlash`, a file-backed flash emulation on the
//! host filesystem. It is NOT an ESP32 or a Raspberry Pi; the flash-read counts
//! are device-model exact, the milliseconds are host-specific.

use slate_kv::{Db, KeySource, MountReport, Options, Profile};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Volume size. Capped by the index's 24-bit offset field: `epoch::mount`
/// rejects any `capacity > 1 << OFF_BITS`, and `Db::open` translates that
/// rejection into "format a new volume" — so an oversized capacity makes every
/// reopen silently reformat, replay nothing, and report a mount cost that looks
/// beautifully flat while measuring nothing at all. 16 MiB is exactly the
/// ceiling. `assert_capacity_mountable` below fails loudly if this is raised.
const CAPACITY: u32 = 16 * 1024 * 1024;
const KEY_LEN: usize = 12;
const VAL_LEN: usize = 64;
/// Distinct keys the workload cycles through. Bounded so the index stays small
/// (and so `n_keys` need not grow with the volume sweep), while the *log* still
/// grows monotonically with the number of writes — which is the variable the
/// volume sweep needs to move.
const N_DISTINCT: usize = 512;
const N_KEYS_OPT: usize = 1024;
const REPEATS: usize = 5;

const TAIL_SWEEP: [usize; 8] = [0, 10, 50, 100, 500, 1000, 4000, 8000];
const VOLUME_SWEEP: [usize; 6] = [0, 100, 1000, 5000, 20_000, 40_000];
const FIXED_TAIL: usize = 200;
const FIXED_BASE: usize = 2000;

/// Refuses to run if `CAPACITY` is one a real mount would reject.
fn assert_capacity_mountable() {
    let ceiling: u64 = 1u64 << slate_kv_core::config::OFF_BITS;
    assert!(
        CAPACITY as u64 <= ceiling,
        "CAPACITY={CAPACITY} exceeds the {ceiling}-byte offset ceiling; mount would return \
         FormatError and every reopen would reformat instead of replaying"
    );
}

fn opts() -> Options {
    Options {
        capacity: CAPACITY,
        b_commit: 8,
        auto_b: false,
        staleness_budget_ms: 1000,
        n_keys: N_KEYS_OPT,
        profile: Profile::Pi,
        // Mount performs no `program`, so the fsync policy cannot affect the
        // measured mount cost; `OsCache` only makes the multi-40k-record setup
        // phase finish in minutes rather than hours.
        durability: slate_kv::file_flash::Durability::OsCache,
    }
}

fn key_of(i: usize) -> [u8; KEY_LEN] {
    let mut k = [b'k'; KEY_LEN];
    let s = format!("{:0w$}", i % N_DISTINCT, w = KEY_LEN - 1);
    k[1..].copy_from_slice(s.as_bytes());
    k
}

/// Builds a volume with `base` records, a checkpoint, then `tail` more durable
/// records. Returns (log head at seal time, log head at close, keys live).
fn build(dir: &Path, base: usize, tail: usize) -> (u32, u32, usize) {
    let db = Db::open(dir, KeySource::Bytes([0x5Au8; 32]), opts()).expect("open for build");
    let val = vec![0xA5u8; VAL_LEN];

    for i in 0..base {
        db.put(&key_of(i), &val).expect("put base");
    }
    db.seal_epoch().expect("seal");
    let seal_head = db.stats().hot_head;

    for i in 0..tail {
        db.put_durable(&key_of(base + i), &val).expect("put tail");
    }
    db.commit().expect("commit tail");
    let close_head = db.stats().hot_head;
    let keys = db.len();
    drop(db);
    (seal_head, close_head, keys)
}

/// Reopens the volume `REPEATS` times, timing each mount. Returns
/// (report from the first mount, per-reopen milliseconds).
fn time_mounts(dir: &Path) -> (MountReport, Vec<f64>) {
    let mut ms = Vec::with_capacity(REPEATS);
    let mut first: Option<MountReport> = None;
    for _ in 0..REPEATS {
        let t0 = Instant::now();
        let db = Db::open(dir, KeySource::Bytes([0x5Au8; 32]), opts()).expect("reopen");
        let dt = t0.elapsed();
        let rep = db.mount_report();
        drop(db);
        ms.push(dt.as_secs_f64() * 1e3);
        if first.is_none() {
            first = Some(rep);
        }
    }
    (first.expect("at least one reopen"), ms)
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).expect("no NaN timings"));
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

#[allow(clippy::too_many_arguments)]
fn emit(
    experiment: &str,
    base: usize,
    tail: usize,
    seal_head: u32,
    close_head: u32,
    keys_at_close: usize,
    rep: &MountReport,
    ms: &[f64],
) {
    let med = median(ms);
    let min = ms.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = ms.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let tail_intact = rep.replay_from == seal_head && rep.records_replayed == tail as u64;
    let per_rec_us = if tail > 0 {
        med * 1e3 / tail as f64
    } else {
        f64::NAN
    };
    println!(
        "{experiment},{base},{tail},{seal_head},{close_head},{},{},{},{},{},{},{},{},\
         {},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{:.4},{:.4},{:.4},{},{:.4}",
        close_head - slate_kv_core::config::data_base_offset(4096),
        keys_at_close,
        u8::from(rep.had_checkpoint),
        rep.replay_from,
        rep.head_pos,
        rep.scan_bytes,
        rep.records_replayed,
        u8::from(tail_intact),
        rep.keys,
        rep.ckpt_index_bytes,
        rep.index_slots,
        rep.ckpt_slots_verified,
        rep.flash_after_ckpt.read_ops,
        rep.flash_after_ckpt.read_bytes,
        rep.flash_after_ckpt.read_pages,
        rep.flash.read_ops - rep.flash_after_ckpt.read_ops,
        rep.flash.read_bytes - rep.flash_after_ckpt.read_bytes,
        rep.flash.read_pages - rep.flash_after_ckpt.read_pages,
        rep.key_verify_calls,
        rep.flash.read_ops,
        rep.flash.read_bytes,
        rep.flash.read_pages,
        rep.flash.program_ops,
        rep.flash.erase_ops,
        med,
        min,
        max,
        REPEATS,
        per_rec_us,
    );
}

fn main() {
    let root: PathBuf = std::env::temp_dir().join(format!(
        "slate_paper_recovery_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos()
    ));

    assert_capacity_mountable();
    println!("# SLATE paper measurement: mount (recovery) cost");
    println!(
        "# cmd=cargo run --release -p slate-kv --example paper_recovery \
         backend=FileFlash(file-backed emulation) capacity={CAPACITY} page=256 block=4096 \
         durability=OsCache b_commit=8 auto_b=false n_keys={N_KEYS_OPT} \
         key_len={KEY_LEN} val_len={VAL_LEN} n_distinct_keys={N_DISTINCT} \
         THETA={} data_base={} MAX_CKPT_LEN={} repeats={REPEATS} \
         platform=macOS-26.5.2/arm64/12-core/24GiB",
        slate_kv_core::config::THETA,
        slate_kv_core::config::data_base_offset(4096),
        slate_kv_core::config::MAX_CKPT_LEN
    );
    println!(
        "# tail_sweep: base={FIXED_BASE} fixed, tail swept. \
         volume_sweep: tail={FIXED_TAIL} fixed, base swept. \
         tail_intact=0 means an unplanned epoch seal moved the checkpoint, so that row \
         is NOT the requested operating point. median/min/max are over {REPEATS} \
         reopens of the same volume; the first is cold-cache, so max is usually it."
    );
    println!(
        "experiment,base_records,tail_records_requested,seal_head,close_head,log_bytes_total,\
         keys_at_close,had_checkpoint,replay_from,head_pos,scan_bytes,records_replayed,\
         tail_intact,keys_after_mount,ckpt_index_bytes,index_slots,\
         ckpt_slots_verified,ckpt_read_ops,ckpt_read_bytes,ckpt_read_pages,\
         replay_read_ops,replay_read_bytes,replay_read_pages,key_verify_calls,\
         mount_read_ops,mount_read_bytes,mount_read_pages,mount_program_ops,mount_erase_ops,\
         mount_ms_median,mount_ms_min,mount_ms_max,repeats,mount_us_per_replayed_record"
    );

    for &tail in &TAIL_SWEEP {
        let dir = root.join(format!("tail_{tail}"));
        let _ = std::fs::remove_dir_all(&dir);
        let (seal_head, close_head, keys) = build(&dir, FIXED_BASE, tail);
        let (rep, ms) = time_mounts(&dir);
        assert!(
            rep.had_checkpoint,
            "tail={tail}: mount found no checkpoint, so it reformatted instead of replaying"
        );
        emit(
            "tail_sweep",
            FIXED_BASE,
            tail,
            seal_head,
            close_head,
            keys,
            &rep,
            &ms,
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    for &base in &VOLUME_SWEEP {
        let dir = root.join(format!("vol_{base}"));
        let _ = std::fs::remove_dir_all(&dir);
        let (seal_head, close_head, keys) = build(&dir, base, FIXED_TAIL);
        let (rep, ms) = time_mounts(&dir);
        assert!(
            rep.had_checkpoint,
            "base={base}: mount found no checkpoint, so it reformatted instead of replaying"
        );
        emit(
            "volume_sweep",
            base,
            FIXED_TAIL,
            seal_head,
            close_head,
            keys,
            &rep,
            &ms,
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    let _ = std::fs::remove_dir_all(&root);
}
