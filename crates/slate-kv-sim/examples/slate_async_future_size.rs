//! Measures `size_of` of every public async engine future in `slate-kv-core`,
//! plus the `ScratchWorkspace` / `Slate` structs the buffers were hoisted into.
//!
//! Doc 018 §5 claims every public engine future stays under `MAX_FUTURE_BYTES`
//! (2048 B) because large stack buffers live in an externally-owned
//! `ScratchWorkspace` rather than in the future. This example measures the real
//! numbers instead of trusting them, and demonstrates the counterfactual (a
//! stack-local buffer held across an await vs the same buffer borrowed).
//!
//! Output: CSV on stdout, `operation,future_bytes,...`.

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

type Eng<'a> = Slate<'a, SimFlash, SimCounter, CryptoSealer>;
type BlockingEng<'a> = Slate<
    'a,
    slate_kv_hal::BlockingFlash<SimFlash>,
    slate_kv_hal::BlockingCounter<SimCounter>,
    CryptoSealer,
>;

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
fn build<'a>(
    flash: SimFlash,
    counter: SimCounter,
    hot: &'a mut [u8],
    cold: &'a mut [u8],
    slots: &'a mut [u32],
    ckpt: &'a mut [u8],
) -> Eng<'a> {
    let data_base = data_base_offset(BLOCK);
    let n_segs = slate_kv_core::gc::segments_in(data_base, 4096 * 512);
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

/// Counterfactual A: a 1280-byte buffer as a stack local held across an await.
async fn local_buffer_variant<F: slate_kv_hal::AsyncFlash>(flash: &mut F, addr: u32) -> u8 {
    let mut scratch = [0u8; MAX_KEY_LEN + MAX_VAL_LEN];
    let _ = flash.read(addr, &mut scratch[..1]).await;
    slate_kv_core::task::yield_now().await;
    scratch[0]
}

/// Counterfactual B: the same routine with the buffer borrowed from outside.
async fn borrowed_buffer_variant<F: slate_kv_hal::AsyncFlash>(
    flash: &mut F,
    addr: u32,
    scratch: &mut [u8],
) -> u8 {
    let _ = flash.read(addr, &mut scratch[..1]).await;
    slate_kv_core::task::yield_now().await;
    scratch[0]
}

fn row(op: &str, bytes: usize) {
    // MAX_FUTURE_BYTES does not exist in the source tree; 2048 is the bound the
    // design doc states, applied here as an external check.
    let bound = 2048usize;
    println!(
        "{},{},{},{}",
        op,
        bytes,
        bound,
        if bytes <= bound { "yes" } else { "NO" }
    );
}

fn main() {
    println!("operation,future_bytes,claimed_bound_bytes,under_bound");

    let mut hot = vec![0u8; 65536];
    let mut cold = vec![0u8; 65536];
    let mut slots = vec![0u32; N_BUCKETS * BUCKET_SLOTS];
    let mut ckpt = vec![0u8; MAX_CKPT_LEN as usize];
    let flash = SimFlash::new(4096 * 512, PAGE, BLOCK);
    let counter = SimCounter::new(1_000_000);
    let mut st = build(flash, counter, &mut hot, &mut cold, &mut slots, &mut ckpt);

    // --- public Slate async surface (SimFlash monomorphisation) -------------
    {
        let mut out = [0u8; MAX_VAL_LEN];
        let f = st.get_into_async(b"k", &mut out);
        row("Slate::get_into_async", core::mem::size_of_val(&f));
        drop(f);
    }
    {
        let f = st.index_update_offset_async(b"k", 4096);
        row(
            "Slate::index_update_offset_async",
            core::mem::size_of_val(&f),
        );
        drop(f);
    }
    {
        let f = st.index_remove_key_async(b"k");
        row("Slate::index_remove_key_async", core::mem::size_of_val(&f));
        drop(f);
    }
    {
        let f = st.append_cold_async(b"k", b"v", 0);
        row("Slate::append_cold_async", core::mem::size_of_val(&f));
        drop(f);
    }
    {
        let f = st.append_cold_tombstone_async(b"k", 0);
        row(
            "Slate::append_cold_tombstone_async",
            core::mem::size_of_val(&f),
        );
        drop(f);
    }
    {
        let f = st.commit_async();
        row("Slate::commit_async", core::mem::size_of_val(&f));
        drop(f);
    }
    {
        let f = st.seal_epoch_now_async();
        row("Slate::seal_epoch_now_async", core::mem::size_of_val(&f));
        drop(f);
    }
    {
        let f = st.compact_async();
        row("Slate::compact_async", core::mem::size_of_val(&f));
        drop(f);
    }
    {
        let f = slate_kv_core::gc::compact_one_async(&mut st);
        row("gc::compact_one_async", core::mem::size_of_val(&f));
        drop(f);
    }
    {
        let f = st
            .log_hot
            .commit_async(&mut st.flash, &mut st.sealer, &st.engine.chain, 1, 1);
        row("Log::commit_async", core::mem::size_of_val(&f));
        drop(f);
    }
    {
        let f = slate_kv_core::segment::encode_parity(
            &mut st.flash,
            &slate_kv_core::segment::Segment {
                start_addr: 0,
                block_size: BLOCK as u32,
            },
        );
        row("segment::encode_parity", core::mem::size_of_val(&f));
        drop(f);
    }

    // epoch layer: needs a separate borrow set
    {
        let mut page_buf = [0u8; MAX_PAGE_SIZE];
        let f = slate_kv_core::epoch::seal_epoch_async(
            &mut st.engine,
            &mut st.flash,
            &mut st.counter,
            &mut st.sealer,
            1,
            0,
            0,
            st.ckpt_buf,
            0,
            &mut page_buf,
        );
        row("epoch::seal_epoch_async", core::mem::size_of_val(&f));
        drop(f);
    }
    {
        let mut out = vec![0u8; MAX_CKPT_LEN as usize];
        let f = slate_kv_core::epoch::mount_async(
            &mut st.flash,
            &mut st.counter,
            &mut st.sealer,
            &mut out,
        );
        row("epoch::mount_async", core::mem::size_of_val(&f));
        drop(f);
    }

    // --- counterfactual ----------------------------------------------------
    {
        let f = local_buffer_variant(&mut st.flash, 0);
        row(
            "counterfactual::stack_local_1280B_across_await",
            core::mem::size_of_val(&f),
        );
        drop(f);
    }
    {
        let mut scratch = vec![0u8; MAX_KEY_LEN + MAX_VAL_LEN];
        let f = borrowed_buffer_variant(&mut st.flash, 0, &mut scratch);
        row(
            "counterfactual::borrowed_1280B_buffer",
            core::mem::size_of_val(&f),
        );
        drop(f);
    }
    row(
        "task::YieldNow",
        core::mem::size_of::<slate_kv_core::task::YieldNow>(),
    );

    // --- where the RAM actually went ---------------------------------------
    row(
        "struct::ScratchWorkspace",
        core::mem::size_of::<ScratchWorkspace>(),
    );
    row(
        "struct::Slate<SimFlash,SimCounter,CryptoSealer>",
        core::mem::size_of::<Eng<'static>>(),
    );
    row(
        "struct::Slate<BlockingFlash<SimFlash>,BlockingCounter<SimCounter>,CryptoSealer>",
        core::mem::size_of::<BlockingEng<'static>>(),
    );
    row("struct::SimFlash", core::mem::size_of::<SimFlash>());
    row("struct::CryptoSealer", core::mem::size_of::<CryptoSealer>());
    row("struct::EngineState", core::mem::size_of::<EngineState>());
}
