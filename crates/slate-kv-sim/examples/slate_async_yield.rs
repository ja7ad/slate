//! Measures the uninterruptible span between consecutive yield points on every
//! flash-touching async path, under simulated W25Q-class SPI-NOR latencies.
//!
//! `SimFlash`'s own `read_lat_ms`/`prog_lat_ms`/`erase_lat_ms` fields are `u64`
//! milliseconds and are never consulted by any code path in the workspace, so a
//! 0.1 ms page read cannot be expressed at all. This harness therefore wraps
//! `SimFlash` in a microsecond-resolution latency-accounting adapter and drives
//! the futures with its own poll loop, treating every `Poll::Pending` as a yield
//! boundary (the only `Pending` a `BlockingFlash`-style adapter can produce is
//! `task::yield_now`).
//!
//! Latency model (stated so the paper can cite it):
//!   read   100 us  — 256 B page over SPI at 40 MHz plus command overhead
//!   program 500 us — W25Q tPP typical (0.4-0.5 ms)
//!   erase 45000 us — W25Q tSE typical for a 4 KiB sector (max is 400 ms)
//!
//! Output: CSV on stdout.

use core::cell::Cell;
use core::future::Future;
use core::task::{Context, Poll, Waker};
use std::rc::Rc;

use slate_kv_core::config::*;
use slate_kv_core::epoch::{EngineState, SecurityMode};
use slate_kv_core::index::Index;
use slate_kv_core::log::{HeadState, Log};
use slate_kv_core::sched::Scheduler;
use slate_kv_core::slate::{ScratchWorkspace, Slate};
use slate_kv_crypto::sealer::CryptoSealer;
use slate_kv_sim::{SimCounter, SimFlash};

const PAGE: usize = 256;
const BLOCK: usize = 4096;
const READ_US: u64 = 100;
const PROG_US: u64 = 500;
const ERASE_US: u64 = 45_000;

#[derive(Clone, Default)]
struct Clocks {
    t_us: Rc<Cell<u64>>,
    ops: Rc<Cell<u64>>,
    erases: Rc<Cell<u64>>,
}

struct LatFlash {
    inner: SimFlash,
    c: Clocks,
}

impl LatFlash {
    fn new(inner: SimFlash, c: Clocks) -> Self {
        Self { inner, c }
    }
    fn bump(&self, us: u64) {
        self.c.t_us.set(self.c.t_us.get() + us);
        self.c.ops.set(self.c.ops.get() + 1);
    }
}

impl slate_kv_hal::Flash for LatFlash {
    type Error = slate_kv_sim::SimFlashError;
    fn page_size(&self) -> usize {
        slate_kv_hal::Flash::page_size(&self.inner)
    }
    fn block_size(&self) -> usize {
        slate_kv_hal::Flash::block_size(&self.inner)
    }
    fn capacity(&self) -> u32 {
        slate_kv_hal::Flash::capacity(&self.inner)
    }
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        // Charged per 256 B page: a 32 900 B checkpoint read is not one 100 us
        // SPI transaction.
        let pages = (buf.len().div_ceil(PAGE)).max(1) as u64;
        self.bump(READ_US * pages);
        slate_kv_hal::Flash::read(&mut self.inner, addr, buf)
    }
    fn program(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        let pages = (buf.len() / PAGE).max(1) as u64;
        self.bump(PROG_US * pages);
        slate_kv_hal::Flash::program(&mut self.inner, addr, buf)
    }
    fn erase(&mut self, block_addr: u32) -> Result<(), Self::Error> {
        self.bump(ERASE_US);
        self.c.erases.set(self.c.erases.get() + 1);
        slate_kv_hal::Flash::erase(&mut self.inner, block_addr)
    }
}

impl slate_kv_hal::AsyncFlash for LatFlash {
    type Error = slate_kv_sim::SimFlashError;
    fn page_size(&self) -> usize {
        slate_kv_hal::Flash::page_size(&self.inner)
    }
    fn block_size(&self) -> usize {
        slate_kv_hal::Flash::block_size(&self.inner)
    }
    fn capacity(&self) -> u32 {
        slate_kv_hal::Flash::capacity(&self.inner)
    }
    async fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        slate_kv_hal::Flash::read(self, addr, buf)
    }
    async fn program(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        slate_kv_hal::Flash::program(self, addr, buf)
    }
    async fn erase(&mut self, block_addr: u32) -> Result<(), Self::Error> {
        slate_kv_hal::Flash::erase(self, block_addr)
    }
}

struct LatCounter {
    inner: SimCounter,
    c: Clocks,
}

impl slate_kv_hal::MonotonicCounter for LatCounter {
    type Error = slate_kv_sim::SimCounterError;
    fn kind(&self) -> slate_kv_hal::CounterKind {
        slate_kv_hal::MonotonicCounter::kind(&self.inner)
    }
    fn read(&mut self) -> Result<u64, Self::Error> {
        slate_kv_hal::MonotonicCounter::read(&mut self.inner)
    }
    fn increment(&mut self) -> Result<u64, Self::Error> {
        // An RTC/I2C-backed counter bump is a durable off-chip write; charge it
        // one page program.
        self.c.t_us.set(self.c.t_us.get() + PROG_US);
        self.c.ops.set(self.c.ops.get() + 1);
        slate_kv_hal::MonotonicCounter::increment(&mut self.inner)
    }
}

impl slate_kv_hal::AsyncMonotonicCounter for LatCounter {
    type Error = slate_kv_sim::SimCounterError;
    fn kind(&self) -> slate_kv_hal::CounterKind {
        slate_kv_hal::MonotonicCounter::kind(&self.inner)
    }
    async fn read(&mut self) -> Result<u64, Self::Error> {
        slate_kv_hal::MonotonicCounter::read(self)
    }
    async fn increment(&mut self) -> Result<u64, Self::Error> {
        slate_kv_hal::MonotonicCounter::increment(self)
    }
}

type Eng<'a> = Slate<'a, LatFlash, LatCounter, CryptoSealer>;

/// One inter-yield span.
#[derive(Debug, Clone, Copy)]
struct Span {
    us: u64,
    ops: u64,
    erases: u64,
}

/// Drives `fut` to completion, recording the simulated time, flash-op count and
/// erase count of every span between consecutive `Poll::Pending` returns.
fn drive<T>(fut: impl Future<Output = T>, c: &Clocks) -> (T, Vec<Span>) {
    let mut fut = core::pin::pin!(fut);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut spans = Vec::new();
    let mut last = (c.t_us.get(), c.ops.get(), c.erases.get());
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Pending => {
                let now = (c.t_us.get(), c.ops.get(), c.erases.get());
                spans.push(Span {
                    us: now.0 - last.0,
                    ops: now.1 - last.1,
                    erases: now.2 - last.2,
                });
                last = now;
            }
            Poll::Ready(v) => {
                let now = (c.t_us.get(), c.ops.get(), c.erases.get());
                spans.push(Span {
                    us: now.0 - last.0,
                    ops: now.1 - last.1,
                    erases: now.2 - last.2,
                });
                return (v, spans);
            }
        }
    }
}

fn sealer() -> CryptoSealer {
    let dk = slate_kv_crypto::keys::DeviceKey([7u8; 32]);
    CryptoSealer::new(slate_kv_crypto::keys::KeySet::derive(&dk, 1))
}

fn build<'a>(
    c: &Clocks,
    cap: u32,
    hot: &'a mut [u8],
    cold: &'a mut [u8],
    slots: &'a mut [u32],
    ckpt: &'a mut [u8],
) -> Eng<'a> {
    let data_base = data_base_offset(BLOCK);
    let n_segs = slate_kv_core::gc::segments_in(data_base, cap);
    Slate {
        flash: LatFlash::new(SimFlash::new(cap, PAGE, BLOCK), c.clone()),
        counter: LatCounter {
            inner: SimCounter::new(1_000_000),
            c: c.clone(),
        },
        sealer: sealer(),
        engine: EngineState {
            epoch: 1,
            next_seq: 1,
            acked_seq: 0,
            d_ckpt: [0u8; 32],
            chain: slate_kv_core::chain::Chain::anchor(1, &[0u8; 32]),
            records_in_epoch: 0,
            security_mode: SecurityMode::Full,
            active_ckpt_slot: 0,
        },
        log_hot: Log::new(
            hot,
            HeadState {
                seg_seq: 1,
                write_offset: data_base,
                block_idx: data_base / BLOCK as u32,
            },
        ),
        log_cold: Log::new(
            cold,
            HeadState {
                seg_seq: 2,
                write_offset: data_base,
                block_idx: data_base / BLOCK as u32,
            },
        ),
        index: Index::new(slots, N_BUCKETS),
        segs: slate_kv_core::gc::SegTable::with_base(data_base, n_segs),
        ckpt_seg_seq: 0,
        sched: Scheduler::new(SchedCfg {
            auto_b: false,
            fixed_cost_uj: 0,
            staleness_budget_ms: 1000,
            deadline_ms: 1000,
            b_min: 1,
            b_max: 128,
            b_commit: 8,
        }),
        metrics: slate_kv_core::metrics::Metrics::default(),
        ckpt_buf: ckpt,
        rng: slate_kv_core::index::XorShift64::new(42),
        scratch_buf: ScratchWorkspace::new(),
    }
}

fn emit(path: &str, spans: &[Span]) {
    if spans.is_empty() {
        println!("{},0,,,,,,,", path);
        return;
    }
    let n = spans.len() as u64;
    let yields = n.saturating_sub(1); // last span ends in Ready, not a yield
    let total: u64 = spans.iter().map(|s| s.us).sum();
    let mean = total / n;
    let max = spans.iter().map(|s| s.us).max().unwrap();
    let worst = spans.iter().max_by_key(|s| s.us).unwrap();
    // Span excluding the one indivisible erase the doc's acceptance criterion
    // exempts: subtract one erase from the worst span if it contains any.
    let max_excl_one_erase = if worst.erases > 0 {
        worst.us.saturating_sub(ERASE_US)
    } else {
        worst.us
    };
    let total_ops: u64 = spans.iter().map(|s| s.ops).sum();
    let total_erases: u64 = spans.iter().map(|s| s.erases).sum();
    println!(
        "{},{},{},{},{:.3},{:.3},{:.3},{},{},{}",
        path,
        yields,
        n,
        total_ops,
        total as f64 / 1000.0,
        mean as f64 / 1000.0,
        max as f64 / 1000.0,
        worst.ops,
        worst.erases,
        format_args!("{:.3}", max_excl_one_erase as f64 / 1000.0),
    );
    let _ = (max, total_erases);
}

fn main() {
    let cap: u32 = 4096 * 512; // 2 MiB, the ESP32 region size
    println!(
        "path,yield_points,spans,flash_ops_total,total_sim_ms,mean_span_ms,max_span_ms,\
ops_in_longest_span,erases_in_longest_span,max_span_ms_excl_one_erase"
    );

    // ---- 1. commit -------------------------------------------------------
    {
        let c = Clocks::default();
        let mut hot = vec![0u8; 65536];
        let mut cold = vec![0u8; 65536];
        let mut slots = vec![0u32; N_BUCKETS * BUCKET_SLOTS];
        let mut ckpt = vec![0u8; MAX_CKPT_LEN as usize];
        let mut st = build(&c, cap, &mut hot, &mut cold, &mut slots, &mut ckpt);
        for i in 0..8u32 {
            let k = format!("commit_key_{i:04}");
            let _ = st.append_hot(OP_PUT, k.as_bytes(), &[0xAAu8; 64]);
        }
        c.t_us.set(0);
        c.ops.set(0);
        c.erases.set(0);
        let (r, spans) = drive(st.commit_async(), &c);
        assert!(r.is_ok(), "commit failed: {r:?}");
        emit("Slate::commit_async[8 records]", &spans);
    }

    // ---- 2. seal_epoch_now ----------------------------------------------
    {
        let c = Clocks::default();
        let mut hot = vec![0u8; 65536];
        let mut cold = vec![0u8; 65536];
        let mut slots = vec![0u32; N_BUCKETS * BUCKET_SLOTS];
        let mut ckpt = vec![0u8; MAX_CKPT_LEN as usize];
        let mut st = build(&c, cap, &mut hot, &mut cold, &mut slots, &mut ckpt);
        for i in 0..64u32 {
            let k = format!("seal_key_{i:04}");
            let off = st.append_hot(OP_PUT, k.as_bytes(), &[0xBBu8; 64]).unwrap();
            let _ = slate_kv_core::task::block_on(st.index_update_offset_async(k.as_bytes(), off));
        }
        let _ = slate_kv_core::task::block_on(st.commit_async());
        c.t_us.set(0);
        c.ops.set(0);
        c.erases.set(0);
        let (r, spans) = drive(st.seal_epoch_now_async(), &c);
        assert!(r.is_ok(), "seal failed: {r:?}");
        emit("Slate::seal_epoch_now_async[8192-slot index]", &spans);
    }

    // ---- 3. GC compaction ------------------------------------------------
    {
        let c = Clocks::default();
        let mut hot = vec![0u8; 65536];
        let mut cold = vec![0u8; 65536];
        let mut slots = vec![0u32; N_BUCKETS * BUCKET_SLOTS];
        let mut ckpt = vec![0u8; MAX_CKPT_LEN as usize];
        let mut st = build(&c, cap, &mut hot, &mut cold, &mut slots, &mut ckpt);
        // Fill enough to populate at least one whole segment, then seal so a
        // victim becomes eligible.
        let mut written = 0u32;
        for i in 0..600u32 {
            let k = format!("gc_key_{i:05}");
            match st.append_hot(OP_PUT, k.as_bytes(), &[0xCCu8; 256]) {
                Ok(off) => {
                    let _ = slate_kv_core::task::block_on(
                        st.index_update_offset_async(k.as_bytes(), off),
                    );
                    written += 1;
                }
                Err(_) => break,
            }
            if i % 8 == 7 && slate_kv_core::task::block_on(st.commit_async()).is_err() {
                break;
            }
        }
        let _ = slate_kv_core::task::block_on(st.commit_async());
        let _ = slate_kv_core::task::block_on(st.seal_epoch_now_async());
        eprintln!(
            "gc setup: {written} records, segs={} sealed={} hot_head={}",
            st.segs.num_segments,
            st.segs.count_in_state(slate_kv_core::gc::SegState::Sealed),
            st.log_hot.head.write_offset
        );
        c.t_us.set(0);
        c.ops.set(0);
        c.erases.set(0);
        let (r, spans) = drive(slate_kv_core::gc::compact_one_async(&mut st), &c);
        eprintln!("gc result: {r:?} erases={}", c.erases.get());
        emit(
            &format!("gc::compact_one_async[GC_YIELD_EVERY_RECORDS={GC_YIELD_EVERY_RECORDS}]"),
            &spans,
        );
    }

    // ---- 4. mount + recovery (the boot path) -----------------------------
    // Swept over tail size so the paper can state a measured slope for the
    // recovery span rather than extrapolating from one point. `recover` is on
    // the blocking `Flash` trait, so its span grows linearly and unyieldably.
    // THETA = 16384 is the epoch-seal trigger, i.e. the largest tail the design
    // permits a mount to face, so it is measured directly rather than
    // extrapolated. It needs more than the 2 MiB ESP32 region to fit.
    for tail_target in [128u32, 256, 512, 1024, 2048, 4096, 8192, THETA as u32] {
        let c = Clocks::default();
        let mut hot = vec![0u8; 65536];
        let mut cold = vec![0u8; 65536];
        let mut slots = vec![0u32; N_BUCKETS * BUCKET_SLOTS];
        let mut ckpt = vec![0u8; MAX_CKPT_LEN as usize];
        let boot_cap: u32 = 4096 * 4096; // 16 MiB, so a THETA-sized tail fits
        let mut st = build(&c, boot_cap, &mut hot, &mut cold, &mut slots, &mut ckpt);
        // Write a checkpoint first, so the mount has one to load and the tail
        // replay starts where sim_db.rs/db.rs start it.
        for i in 0..64u32 {
            let k = format!("boot_key_{i:05}");
            let off = st.append_hot(OP_PUT, k.as_bytes(), &[0xDDu8; 64]).unwrap();
            let _ = slate_kv_core::task::block_on(st.index_update_offset_async(k.as_bytes(), off));
        }
        let _ = slate_kv_core::task::block_on(st.commit_async());
        let _ = slate_kv_core::task::block_on(st.seal_epoch_now_async());
        // Tail: records committed AFTER the checkpoint — what a mount replays.
        let mut tail_records = 0u32;
        for i in 0..tail_target {
            let k = format!("tail_key_{i:05}");
            match st.append_hot(OP_PUT, k.as_bytes(), &[0xEEu8; 64]) {
                Ok(_) => tail_records += 1,
                Err(_) => break,
            }
            if i % 8 == 7 && slate_kv_core::task::block_on(st.commit_async()).is_err() {
                break;
            }
        }
        let _ = slate_kv_core::task::block_on(st.commit_async());

        let mut out = vec![0u8; MAX_CKPT_LEN as usize];
        let mut s2 = sealer();
        c.t_us.set(0);
        c.ops.set(0);
        c.erases.set(0);
        let (mi, spans) = drive(
            slate_kv_core::epoch::mount_async(&mut st.flash, &mut st.counter, &mut s2, &mut out),
            &c,
        );
        let mi = mi.expect("mount failed");
        if tail_target == 1024 {
            emit("epoch::mount_async[checkpoint load only]", &spans);
        }

        // recover(): still on the BLOCKING `Flash` trait, so it cannot yield at
        // all. One single uninterruptible span, by construction. Chain/epoch/
        // start offset are wired exactly as sim_db.rs and db.rs wire them.
        let replay_from = mi.ckpt_write_offset.max(data_base_offset(BLOCK));
        let mut chain = mi.state.chain;
        let mut ws = Box::new(slate_kv_core::recover::RecoverWorkspace::new());
        let mut s3 = sealer();
        c.t_us.set(0);
        c.ops.set(0);
        c.erases.set(0);
        let info = slate_kv_core::recover::recover(
            &mut st.flash,
            &mut s3,
            &mut chain,
            mi.state.epoch,
            replay_from,
            &mut ws,
            |_f, _s, _seq, _off, _op, _k| {},
        );
        let applied = info.as_ref().map(|i| i.records_applied).unwrap_or(0);
        if applied != tail_records as u64 {
            // A tail this long makes the engine seal an intervening checkpoint
            // (reserve_space_async -> seal_epoch_now_async), so `mount` loads
            // that newer checkpoint and the replayable tail is shorter than what
            // was written. Report the point rather than a fabricated one.
            eprintln!(
                "tail_target={tail_target}: wrote {tail_records}, replayable tail {applied} \
(intervening checkpoint at replay_from={replay_from})"
            );
        }
        if applied == 0 {
            eprintln!("tail_target={tail_target}: SKIPPED, no replayable tail");
            continue;
        }
        let sp = [Span {
            us: c.t_us.get(),
            ops: c.ops.get(),
            erases: c.erases.get(),
        }];
        emit(
            &format!("recover::recover[BLOCKING Flash trait; {applied} records replayed]"),
            &sp,
        );
    }
    eprintln!("note: recovery sweep ran on a 16 MiB region so a THETA-sized tail fits");
}
