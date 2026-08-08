//! Reproduces the ESP32-C3 `kv_demo` read path at the `Slate` level.
//!
//! This exists because the `slate-kv` host tests could NOT have caught the bug
//! reported from the board: `Db::get` in `slate-kv/src/db.rs` has its own
//! hand-rolled hot-batch/cold-batch/flash read loop and never calls
//! `Slate::get_into`. So `get_into` — the accessor `kv_demo` actually uses — had
//! no host coverage at all.
//!
//! Everything here mirrors the firmware: the real `CryptoSealer` (not a mock
//! that returns `Ok(())` from `open_record`), the C3 flash geometry
//! (2 MiB region, 256 B page, 4096 B block), both heads starting at
//! `data_base_offset`, and `SegTable::with_base`.

use slate_kv_core::config::{data_base_offset, SchedCfg, OP_DEL, OP_PUT};
use slate_kv_core::epoch::{EngineState, SecurityMode};
use slate_kv_core::gc::SegTable;
use slate_kv_core::index::Index;
use slate_kv_core::log::{HeadState, Log};
use slate_kv_core::slate::Slate;
use slate_kv_crypto::sealer::CryptoSealer;
use slate_kv_sim::{SimCounter, SimFlash};

const FLASH_LEN: u32 = 0x200000; // SLATE_FLASH_LEN on the C3 demos
const PAGE: usize = 256;
const BLOCK: usize = 4096;
const N_BUCKETS: usize = 2048;

struct Bufs {
    hot: Vec<u8>,
    cold: Vec<u8>,
    index: Vec<u32>,
    ckpt: Vec<u8>,
}

impl Bufs {
    fn new() -> Self {
        Self {
            hot: vec![0u8; 4096],
            cold: vec![0u8; 4096],
            index: vec![0u32; N_BUCKETS * 4],
            ckpt: vec![0u8; 35000],
        }
    }
}

fn make_slate(b: &mut Bufs) -> Slate<'_, SimFlash, SimCounter, CryptoSealer> {
    let flash = SimFlash::new(FLASH_LEN, PAGE, BLOCK);
    let counter = SimCounter::new(1_000_000);

    let dev_key = slate_kv_crypto::keys::DeviceKey([0u8; 32]);
    let keys = slate_kv_crypto::keys::KeySet::derive(&dev_key, 1);
    let sealer = CryptoSealer::new(keys);

    let data_base = data_base_offset(BLOCK);

    let engine = EngineState {
        epoch: 1,
        next_seq: 1,
        acked_seq: 0,
        d_ckpt: [0u8; 32],
        chain: slate_kv_core::chain::Chain::anchor(1, &[0u8; 32]),
        records_in_epoch: 0,
        security_mode: SecurityMode::BestEffortRollback,
        active_ckpt_slot: 0,
    };

    // Exactly as kv_demo does it: both heads at data_base.
    let log_hot = Log::new(
        &mut b.hot[..],
        HeadState {
            seg_seq: 0,
            write_offset: data_base,
            block_idx: data_base / BLOCK as u32,
            ..Default::default()
        },
    );
    let log_cold = Log::new(
        &mut b.cold[..],
        HeadState {
            seg_seq: 1,
            write_offset: data_base,
            block_idx: data_base / BLOCK as u32,
            ..Default::default()
        },
    );

    let num_segments = (FLASH_LEN - data_base) / slate_kv_core::config::SEG_BYTES as u32;

    Slate {
        flash,
        counter,
        sealer,
        engine,
        log_hot,
        log_cold,
        index: Index::new(&mut b.index[..], N_BUCKETS),
        segs: SegTable::with_base(data_base, num_segments),
        ckpt_seg_seq: 0,
        sched: slate_kv_core::sched::Scheduler::new(SchedCfg {
            auto_b: false,
            fixed_cost_uj: 1000,
            staleness_budget_ms: 1000,
            deadline_ms: 1000,
            b_min: 1,
            b_max: 128,
            b_commit: 8,
        }),
        metrics: slate_kv_core::metrics::Metrics::default(),
        ckpt_buf: &mut b.ckpt[..],
        rng: slate_kv_core::index::XorShift64::new(42),
        scratch_buf: slate_kv_core::slate::ScratchWorkspace::new(),
    }
}

fn put(s: &mut Slate<'_, SimFlash, SimCounter, CryptoSealer>, k: &[u8], v: &[u8]) {
    let off = s.append_hot(OP_PUT, k, v).expect("append_hot");
    s.index_update_offset(k, off).expect("index_update_offset");
}

fn get(s: &mut Slate<'_, SimFlash, SimCounter, CryptoSealer>, k: &[u8]) -> Option<Vec<u8>> {
    let mut buf = [0u8; slate_kv_core::config::MAX_VAL_LEN];
    s.get_into(k, &mut buf).map(|n| buf[..n].to_vec())
}

/// The exact board transcript that failed:
///   put sensor_0 25 / put sensor_1 23 / commit / get sensor_0  -> "(not found)"
#[test]
fn board_transcript_put_put_commit_get() {
    let mut b = Bufs::new();
    let mut s = make_slate(&mut b);

    put(&mut s, b"sensor_0", b"25");
    put(&mut s, b"sensor_1", b"23");

    // Readable while still in the batch.
    assert_eq!(
        get(&mut s, b"sensor_0").as_deref(),
        Some(&b"25"[..]),
        "sensor_0 unreadable from the uncommitted batch"
    );

    s.commit().expect("commit");

    // ...and after the commit marker is durable. This is the assertion that
    // failed on the board.
    assert_eq!(
        get(&mut s, b"sensor_0").as_deref(),
        Some(&b"25"[..]),
        "sensor_0 unreadable AFTER commit — the board printed (not found) here"
    );
    assert_eq!(get(&mut s, b"sensor_1").as_deref(), Some(&b"23"[..]));
}

/// Reading back across several commits, so a record is resolved from flash long
/// after its batch was flushed and the head has moved on.
#[test]
fn values_readable_across_many_commits() {
    let mut b = Bufs::new();
    let mut s = make_slate(&mut b);

    for i in 0..200u32 {
        let k = format!("key{i:04}");
        let v = format!("val{i:04}");
        put(&mut s, k.as_bytes(), v.as_bytes());
        if i % 8 == 0 {
            s.commit().expect("commit");
        }
    }
    s.commit().expect("commit");

    for i in 0..200u32 {
        let k = format!("key{i:04}");
        let v = format!("val{i:04}");
        assert_eq!(
            get(&mut s, k.as_bytes()).as_deref(),
            Some(v.as_bytes()),
            "{k} lost after commits"
        );
    }
}

/// An overwrite must return the newest value, and a tombstone must read absent.
#[test]
fn overwrite_and_delete_resolve_correctly() {
    let mut b = Bufs::new();
    let mut s = make_slate(&mut b);

    put(&mut s, b"k", b"v1");
    s.commit().expect("commit");
    put(&mut s, b"k", b"v2");
    s.commit().expect("commit");
    assert_eq!(get(&mut s, b"k").as_deref(), Some(&b"v2"[..]));

    s.append_hot(OP_DEL, b"k", &[]).expect("tombstone");
    s.index_remove_key(b"k");
    s.commit().expect("commit");
    assert_eq!(get(&mut s, b"k"), None, "deleted key must read absent");
}
