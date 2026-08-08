//! Cost of the synchronous projection (`task::block_on` over the async core)
//! versus driving the same futures on a cooperative executor that idles.
//!
//! Doc 018 §3.2 documents `task::block_on` as a busy-poll loop over
//! `Waker::noop()` with `core::hint::spin_loop()`, and §10 asks for a "zero-cost
//! check": the blocking facade within 2 % of the blocking engine. Two quantities
//! are measured, because they can disagree:
//!
//!   (a) WALL time and throughput — what a benchmark shows.
//!   (b) CPU time (`getrusage(RUSAGE_SELF)`, user+sys) — what a battery pays.
//!
//! Two flash models:
//!
//!   * `BlockingFlash<SimFlash>` — the production sync facade. Every op returns
//!     an already-ready future, so no future ever suspends for an I/O reason and
//!     `block_on` never actually spins. This is the "zero-cost" claim's own
//!     configuration, and it is measured against the same futures driven by a
//!     bare poll loop with no `spin_loop` hint.
//!
//!   * `DeadlineFlash` — `erase` suspends for a REAL 45 ms (W25Q tSE typical),
//!     returning `Pending` until the deadline passes, standing in for the
//!     DMA/interrupt-backed QSPI driver of doc 018 §6. Both executors face the
//!     identical 45 ms wait; only what they do during it differs. `block_on`
//!     spins; the cooperative executor parks. Wall time is therefore expected to
//!     MATCH and CPU time is expected to DIVERGE — which is the point.
//!
//! Executor: hand-rolled poll loop. `embassy-executor` was NOT used: it is a
//! dependency of `targets/esp32` only, is not a host dependency of any workspace
//! crate, and its ESP32 time driver does not build for aarch64-apple-darwin.
//!
//! Output: CSV on stdout.

use core::cell::Cell;
use core::future::Future;
use core::task::{Context, Poll, Waker};
use std::rc::Rc;
use std::time::Instant;

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
const CAP: u32 = 4096 * 512;
/// Real time an erase suspends for: W25Q-class tSE typical for a 4 KiB sector.
const ERASE_REAL_US: u64 = 45_000;
/// How long the cooperative executor parks per `Pending`. A real executor sleeps
/// until the completion interrupt; 100 us is a coarse stand-in that keeps wall
/// time within a few percent of the erase deadline while cutting the poll count
/// by orders of magnitude. Any residual wall-time excess over `block_on` is this
/// granularity overshoot, not engine cost.
const PARK_US: u64 = 100;

// ---------------------------------------------------------------------------
// CPU-time accounting
// ---------------------------------------------------------------------------
fn cpu_time_us() -> u64 {
    let mut ru: libc::rusage = unsafe { core::mem::zeroed() };
    // SAFETY: `getrusage` writes into a fully-initialised `rusage` we own.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    assert_eq!(rc, 0, "getrusage failed");
    let u = ru.ru_utime.tv_sec as u64 * 1_000_000 + ru.ru_utime.tv_usec as u64;
    let s = ru.ru_stime.tv_sec as u64 * 1_000_000 + ru.ru_stime.tv_usec as u64;
    u + s
}

// ---------------------------------------------------------------------------
// A flash whose erase genuinely suspends for a real 45 ms
// ---------------------------------------------------------------------------
struct DeadlineFlash {
    inner: SimFlash,
    erase_us: u64,
    deadline: Option<Instant>,
    latch: Option<u32>,
    polls: Rc<Cell<u64>>,
}

impl DeadlineFlash {
    fn new(inner: SimFlash, erase_us: u64, polls: Rc<Cell<u64>>) -> Self {
        Self {
            inner,
            erase_us,
            deadline: None,
            latch: None,
            polls,
        }
    }
}

impl slate_kv_hal::AsyncFlash for DeadlineFlash {
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
        slate_kv_hal::Flash::read(&mut self.inner, addr, buf)
    }
    async fn program(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        slate_kv_hal::Flash::program(&mut self.inner, addr, buf)
    }
    /// Suspends until `erase_us` of real time has passed, then performs the
    /// erase. Modelled as latched into the die: once started it always
    /// completes, per the doc 018 §3 indivisibility note.
    fn erase(
        &mut self,
        block_addr: u32,
    ) -> impl core::future::Future<Output = Result<(), Self::Error>> {
        self.latch = Some(block_addr);
        self.deadline = Some(Instant::now() + std::time::Duration::from_micros(self.erase_us));
        EraseFut { f: self }
    }
}

struct EraseFut<'a> {
    f: &'a mut DeadlineFlash,
}

impl Future for EraseFut<'_> {
    type Output = Result<(), slate_kv_sim::SimFlashError>;
    fn poll(self: core::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        me.f.polls.set(me.f.polls.get() + 1);
        if let Some(d) = me.f.deadline {
            if Instant::now() < d {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        }
        me.f.deadline = None;
        let addr = me.f.latch.take().unwrap_or(0);
        Poll::Ready(slate_kv_hal::Flash::erase(&mut me.f.inner, addr))
    }
}

struct PlainCounter(SimCounter);

impl slate_kv_hal::AsyncMonotonicCounter for PlainCounter {
    type Error = slate_kv_sim::SimCounterError;
    fn kind(&self) -> slate_kv_hal::CounterKind {
        slate_kv_hal::MonotonicCounter::kind(&self.0)
    }
    async fn read(&mut self) -> Result<u64, Self::Error> {
        slate_kv_hal::MonotonicCounter::read(&mut self.0)
    }
    async fn increment(&mut self) -> Result<u64, Self::Error> {
        slate_kv_hal::MonotonicCounter::increment(&mut self.0)
    }
}

// ---------------------------------------------------------------------------
// Executors
// ---------------------------------------------------------------------------
/// The engine's own busy-polling bridge, re-implemented here so the two
/// executors are measured through identical call shapes. Identical to
/// `slate_kv_core::task::block_on`.
fn busy_block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = core::pin::pin!(fut);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => core::hint::spin_loop(),
        }
    }
}

/// A cooperative executor that does not spin: on `Pending` it parks the thread
/// for `park_us` instead of burning CPU. This is what a real executor does
/// (Embassy `WFI`, RTIC idle) and what `block_on` cannot do by construction.
/// `park_us = 0` degenerates to a bare poll loop with no `spin_loop` hint.
fn parking_run<F: Future>(fut: F, park_us: u64, parks: &Cell<u64>) -> F::Output {
    let mut fut = core::pin::pin!(fut);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => {
                parks.set(parks.get() + 1);
                if park_us > 0 {
                    std::thread::sleep(std::time::Duration::from_micros(park_us));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Engine construction
// ---------------------------------------------------------------------------
fn sealer() -> CryptoSealer {
    let dk = slate_kv_crypto::keys::DeviceKey([7u8; 32]);
    CryptoSealer::new(slate_kv_crypto::keys::KeySet::derive(&dk, 1))
}

fn engine_state() -> EngineState {
    EngineState {
        epoch: 1,
        next_seq: 1,
        acked_seq: 0,
        d_ckpt: [0u8; 32],
        chain: slate_kv_core::chain::Chain::anchor(1, &[0u8; 32]),
        records_in_epoch: 0,
        security_mode: SecurityMode::Full,
        active_ckpt_slot: 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn build<'a, F, C>(
    flash: F,
    counter: C,
    hot: &'a mut [u8],
    cold: &'a mut [u8],
    slots: &'a mut [u32],
    ckpt: &'a mut [u8],
) -> Slate<'a, F, C, CryptoSealer>
where
    F: slate_kv_hal::AsyncFlash,
    C: slate_kv_hal::AsyncMonotonicCounter,
{
    let data_base = data_base_offset(BLOCK);
    let n_segs = slate_kv_core::gc::segments_in(data_base, CAP);
    Slate {
        flash,
        counter,
        sealer: sealer(),
        engine: engine_state(),
        log_hot: Log::new(
            hot,
            HeadState {
                seg_seq: 1,
                write_offset: data_base,
                block_idx: data_base / BLOCK as u32,
            ..Default::default()
            },
        ),
        log_cold: Log::new(
            cold,
            HeadState {
                seg_seq: 2,
                write_offset: data_base,
                block_idx: data_base / BLOCK as u32,
            ..Default::default()
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

#[derive(Debug, Clone, Copy)]
struct Sample {
    /// Records the workload actually appended before a fatal error, if any.
    /// Asserted equal to the requested count so a short run cannot silently
    /// flatter the throughput number.
    ops: u64,
    wall_us: u64,
    cpu_us: u64,
    idle_polls: u64,
}

fn main() {
    println!(
        "flash_model,executor,reps,records_per_rep,erases_per_rep,\
median_wall_ms,median_cpu_ms,cpu_over_wall_pct,pending_polls,\
throughput_records_per_s,wall_delta_pct,cpu_delta_pct"
    );

    // ===== model 1: BlockingFlash<SimFlash>, the production sync facade =====
    // Nothing ever suspends here, so this isolates the pure overhead of the
    // block_on projection itself. 20 reps x 4000 records to get out of the
    // timer noise floor.
    let reps: u32 = 41;
    let n: u32 = 4000;
    let mut w_busy = Vec::new();
    let mut c_busy = Vec::new();
    let mut w_bare = Vec::new();
    let mut c_bare = Vec::new();
    // Interleave the arms so any thermal/frequency drift hits both equally.
    for _ in 0..reps {
        let s = run_ready(n, true);
        assert_eq!(s.ops, n as u64, "busy arm ran short");
        w_busy.push(s.wall_us);
        c_busy.push(s.cpu_us);
        let s = run_ready(n, false);
        assert_eq!(s.ops, n as u64, "bare arm ran short");
        w_bare.push(s.wall_us);
        c_bare.push(s.cpu_us);
    }
    let (bw, bc) = (med(&w_busy), med(&c_busy));
    emit(
        "BlockingFlash<SimFlash> (never suspends)",
        "task::block_on (busy poll, spin_loop hint)",
        reps,
        n,
        0,
        bw,
        bc,
        0,
        bw,
        bc,
    );
    emit(
        "BlockingFlash<SimFlash> (never suspends)",
        "bare poll loop (native async, no spin hint)",
        reps,
        n,
        0,
        med(&w_bare),
        med(&c_bare),
        0,
        bw,
        bc,
    );

    // ===== model 2: erase genuinely suspends for a real 45 ms ==============
    let reps2: u32 = 5;
    let n2: u32 = 300;
    let mut pw_busy = Vec::new();
    let mut pc_busy = Vec::new();
    let mut pp_busy = Vec::new();
    let mut pe_busy = 0u64;
    let mut pw_park = Vec::new();
    let mut pc_park = Vec::new();
    let mut pp_park = Vec::new();
    for _ in 0..reps2 {
        let (s, er) = run_suspending(n2, true);
        assert_eq!(s.ops, n2 as u64, "suspending busy arm ran short");
        pw_busy.push(s.wall_us);
        pc_busy.push(s.cpu_us);
        pp_busy.push(s.idle_polls);
        pe_busy = er;
        let (s, _) = run_suspending(n2, false);
        assert_eq!(s.ops, n2 as u64, "suspending parking arm ran short");
        pw_park.push(s.wall_us);
        pc_park.push(s.cpu_us);
        pp_park.push(s.idle_polls);
    }
    let (pbw, pbc) = (med(&pw_busy), med(&pc_busy));
    emit(
        &format!("DeadlineFlash (erase suspends {ERASE_REAL_US} us of real time)"),
        "task::block_on (busy poll, spin_loop hint)",
        reps2,
        n2,
        pe_busy,
        pbw,
        pbc,
        med(&pp_busy),
        pbw,
        pbc,
    );
    emit(
        &format!("DeadlineFlash (erase suspends {ERASE_REAL_US} us of real time)"),
        "parking executor (100 us park per Pending)",
        reps2,
        n2,
        pe_busy,
        med(&pw_park),
        med(&pc_park),
        med(&pp_park),
        pbw,
        pbc,
    );
}

/// Median, not mean: a single scheduler preemption on a shared macOS host adds
/// an outlier that a mean of 20 samples cannot absorb.
fn med(v: &[u64]) -> u64 {
    let mut s = v.to_vec();
    s.sort_unstable();
    s[s.len() / 2]
}

#[allow(clippy::too_many_arguments)]
fn emit(
    model: &str,
    exec: &str,
    reps: u32,
    n: u32,
    erases: u64,
    wall: u64,
    cpu: u64,
    pending_polls: u64,
    base_wall: u64,
    base_cpu: u64,
) {
    let tput = n as f64 * 1_000_000.0 / wall as f64;
    println!(
        "{},{},{},{},{},{:.3},{:.3},{:.1},{},{:.1},{:+.2},{:+.2}",
        model,
        exec,
        reps,
        n,
        erases,
        wall as f64 / 1000.0,
        cpu as f64 / 1000.0,
        cpu as f64 * 100.0 / wall as f64,
        pending_polls,
        tput,
        (wall as f64 - base_wall as f64) * 100.0 / base_wall as f64,
        (cpu as f64 - base_cpu as f64) * 100.0 / base_cpu as f64,
    );
}

/// Workload over `BlockingFlash<SimFlash>`: the production sync facade, where no
/// future ever suspends for an I/O reason.
fn run_ready(n_records: u32, busy: bool) -> Sample {
    let mut hot = vec![0u8; 65536];
    let mut cold = vec![0u8; 65536];
    let mut slots = vec![0u32; N_BUCKETS * BUCKET_SLOTS];
    let mut ckpt = vec![0u8; MAX_CKPT_LEN as usize];
    let flash = slate_kv_hal::BlockingFlash(SimFlash::new(CAP, PAGE, BLOCK));
    let counter = slate_kv_hal::BlockingCounter(SimCounter::new(1_000_000));
    let mut st = build(flash, counter, &mut hot, &mut cold, &mut slots, &mut ckpt);
    let parks = Cell::new(0u64);
    let w0 = Instant::now();
    let c0 = cpu_time_us();
    let mut ops = 0u64;
    for i in 0..n_records {
        let k = format!("key_{i:06}");
        if st.append_hot(OP_PUT, k.as_bytes(), &[0x5Au8; 128]).is_err() {
            break;
        }
        ops += 1;
        if i % 8 == 7 {
            let r = if busy {
                busy_block_on(st.commit_async())
            } else {
                parking_run(st.commit_async(), 0, &parks)
            };
            if r.is_err() {
                break;
            }
        }
    }
    let _ = if busy {
        busy_block_on(st.commit_async())
    } else {
        parking_run(st.commit_async(), 0, &parks)
    };
    Sample {
        ops,
        wall_us: w0.elapsed().as_micros() as u64,
        cpu_us: cpu_time_us() - c0,
        idle_polls: parks.get(),
    }
}

/// Same workload over `DeadlineFlash`, whose erase suspends for a real 45 ms.
/// Returns the sample plus the erase count, so the CSV can state how much real
/// waiting the run contained.
fn run_suspending(n_records: u32, busy: bool) -> (Sample, u64) {
    let mut hot = vec![0u8; 65536];
    let mut cold = vec![0u8; 65536];
    let mut slots = vec![0u32; N_BUCKETS * BUCKET_SLOTS];
    let mut ckpt = vec![0u8; MAX_CKPT_LEN as usize];
    let polls = Rc::new(Cell::new(0u64));
    let flash = DeadlineFlash::new(
        SimFlash::new(CAP, PAGE, BLOCK),
        ERASE_REAL_US,
        polls.clone(),
    );
    let counter = PlainCounter(SimCounter::new(1_000_000));
    let mut st = build(flash, counter, &mut hot, &mut cold, &mut slots, &mut ckpt);
    let parks = Cell::new(0u64);
    let w0 = Instant::now();
    let c0 = cpu_time_us();
    let mut ops = 0u64;
    for i in 0..n_records {
        let k = format!("key_{i:06}");
        if st.append_hot(OP_PUT, k.as_bytes(), &[0x5Au8; 128]).is_err() {
            break;
        }
        ops += 1;
        if i % 8 == 7 {
            let r = if busy {
                busy_block_on(st.commit_async())
            } else {
                parking_run(st.commit_async(), PARK_US, &parks)
            };
            if r.is_err() {
                break;
            }
        }
        // Seal periodically so the workload actually contains erases: a commit
        // alone programs pages and never erases, and erase is the only op this
        // model suspends on.
        if i % 100 == 99 {
            let r = if busy {
                busy_block_on(st.seal_epoch_now_async())
            } else {
                parking_run(st.seal_epoch_now_async(), PARK_US, &parks)
            };
            if r.is_err() {
                break;
            }
        }
    }
    let erases = st.metrics.erases;
    (
        Sample {
            ops,
            wall_us: w0.elapsed().as_micros() as u64,
            cpu_us: cpu_time_us() - c0,
            idle_polls: polls.get(),
        },
        erases,
    )
}
