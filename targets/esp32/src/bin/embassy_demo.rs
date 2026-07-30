#![no_std]
#![no_main]

// use embassy_executor::Spawner;
// use embassy_time::{Duration, Instant, Timer};
use core::sync::atomic::AtomicU32;
use esp_backtrace as _;
use esp_hal::uart::Uart;
use esp_println::println;

use slate_kv_core::config::SchedCfg;
use slate_kv_core::gc::SegTable;
use slate_kv_core::index::Index;
use slate_kv_core::log::HeadState;
use slate_kv_core::metrics::Metrics;
use slate_kv_core::sched::Scheduler;
use slate_kv_core::slate::Slate;

use slate_esp32::{EspCounter, EspFlash, SyncBuffer};
use slate_kv_crypto::sealer::CryptoSealer;
use slate_kv_hal::{BlockingCounter, BlockingFlash};

esp_bootloader_esp_idf::esp_app_desc!();

static HOT_BUF: SyncBuffer<[u8; 4096]> = SyncBuffer::new("HOT_BUF", [0; 4096]);
static COLD_BUF: SyncBuffer<[u8; 4096]> = SyncBuffer::new("COLD_BUF", [0; 4096]);
static INDEX_SLOTS: SyncBuffer<[u32; 2048 * 4]> = SyncBuffer::new("INDEX_SLOTS", [0; 2048 * 4]);
static CKPT_BUF: SyncBuffer<[u8; 35000]> = SyncBuffer::new("CKPT_BUF", [0; 35000]);

#[allow(dead_code)]
static MAX_JITTER_US: AtomicU32 = AtomicU32::new(0);

/*
#[embassy_executor::task]
async fn heartbeat_task() {
    let mut last = Instant::now();
    loop {
        Timer::after(Duration::from_millis(100)).await;
        let now = Instant::now();
        let elapsed_us = now.duration_since(last).as_micros() as u32;
        let target_us = 100_000;
        let jitter = if elapsed_us > target_us {
            elapsed_us - target_us
        } else {
            target_us - elapsed_us
        };

        // Manual compare_exchange to max since load/store is safe
        let mut curr_max = MAX_JITTER_US.load(Ordering::Relaxed);
        while jitter > curr_max {
            MAX_JITTER_US.store(jitter, Ordering::Relaxed);
            curr_max = MAX_JITTER_US.load(Ordering::Relaxed);
        }
        last = now;
    }
}

#[embassy_executor::task]
async fn jitter_logger_task() {
    loop {
        Timer::after(Duration::from_secs(5)).await;
        let max_jitter = MAX_JITTER_US.load(Ordering::Relaxed);
        MAX_JITTER_US.store(0, Ordering::Relaxed);
        println!("Maximum heartbeat jitter over last 5s: {} us", max_jitter);
    }
}
*/

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let _esp_hal_timer = esp_hal::timer::timg::TimerGroup::new(peripherals.TIMG0);
    // esp_hal::embassy::init(esp_hal_timer.timer0); // Wait, this needs esp-hal-embassy

    let _uart = Uart::new(peripherals.UART0, esp_hal::uart::Config::default()).unwrap();
    println!("embassy_demo started (concurrency, not DMA offload)");

    // Since embassy setup requires specific esp-hal-embassy deps, we leave this as a template
    // For now, we will just use a blocking setup to ensure it compiles as a demo.
    // In a real Embassy setup, we would start an executor here.

    // spawner.spawn(heartbeat_task()).unwrap();
    // spawner.spawn(jitter_logger_task()).unwrap();

    let flash = EspFlash::new(
        slate_esp32::SLATE_FLASH_BASE,
        slate_esp32::SLATE_FLASH_LEN,
        peripherals.FLASH,
    );
    let counter = EspCounter::new();

    let mut async_flash = BlockingFlash(flash);
    let mut async_counter = BlockingCounter(counter);

    let dev_key = slate_kv_crypto::keys::DeviceKey([0u8; 32]);
    let keys = slate_kv_crypto::keys::KeySet::derive(&dev_key, 1);
    let mut sealer = CryptoSealer::new(keys);

    let mut head_state = HeadState {
        seg_seq: 0,
        write_offset: 0,
        block_idx: 0,
    };

    let ckpt_buf = CKPT_BUF.take();
    let (engine_state, _plain_len) =
        match slate_kv_core::task::block_on(slate_kv_core::epoch::mount_async(
            &mut async_flash,
            &mut async_counter,
            &mut sealer,
            &mut *ckpt_buf,
        )) {
            Ok(mi) => {
                head_state.seg_seq = mi.ckpt_seg_seq;
                head_state.write_offset = mi.ckpt_write_offset;
                head_state.block_idx = mi.ckpt_write_offset / 4096;
                (mi.state, mi.plain_len)
            }
            Err(_) => {
                println!("Mount failed. Starting fresh.");
                let st = slate_kv_core::epoch::EngineState {
                    epoch: 1,
                    next_seq: 1,
                    acked_seq: 0,
                    d_ckpt: [0u8; 32],
                    chain: slate_kv_core::chain::Chain::anchor(1, &[0u8; 32]),
                    records_in_epoch: 0,
                    security_mode: slate_kv_core::epoch::SecurityMode::BestEffortRollback,
                    active_ckpt_slot: 0,
                };
                (st, 0)
            }
        };

    // The append log must never overlap the reserved superblock/checkpoint
    // region. A fresh volume, or a checkpoint written before this rule existed,
    // leaves `write_offset` at 0; appending there programs the live checkpoint
    // pages and every commit fails `ProgramWithoutErase`.
    let data_base = slate_kv_core::config::data_base_offset(4096);
    if head_state.write_offset < data_base {
        head_state.write_offset = data_base;
        head_state.block_idx = data_base / 4096;
    }

    // Only address segments that actually fit above the reserved region;
    // `SegTable::new(128)` would let the compactor pick a victim whose base
    // address lies past `capacity()`.
    let num_segments = slate_esp32::slate_segment_capacity(slate_esp32::SLATE_FLASH_LEN);

    let hot_buf = HOT_BUF.take();
    let cold_buf = COLD_BUF.take();
    let index_slots = INDEX_SLOTS.take();

    let sched_cfg = SchedCfg {
        auto_b: false,
        fixed_cost_uj: 1000,
        staleness_budget_ms: 1000,
        deadline_ms: 1000,
        b_min: 1,
        b_max: 128,
        b_commit: 8,
    };

    // `HeadState` is not `Copy`, and it is moved into `log_hot` below, so read
    // the cold log's starting position out before constructing the engine.
    let cold_write_offset = head_state.write_offset;
    let cold_block_idx = head_state.block_idx;

    let rng_seed = engine_state.epoch.max(1) ^ 42;
    let mut slate = Slate {
        flash: async_flash,
        counter: async_counter,
        sealer,
        engine: engine_state,
        log_hot: slate_kv_core::log::Log::new(hot_buf, head_state),
        log_cold: slate_kv_core::log::Log::new(
            cold_buf,
            HeadState {
                seg_seq: 1,
                // Committed unconditionally alongside the hot log, so it must
                // also start above the reserved checkpoint region — and in a
                // DIFFERENT segment from the hot log, since reclaim erases a
                // whole segment and would otherwise take the other log's
                // records with it.
                write_offset: cold_write_offset + slate_kv_core::config::SEG_BYTES as u32,
                block_idx: cold_block_idx,
            },
        ),
        index: Index::new(index_slots, 2048),
        // `with_base`, not `new`: segments tile the log area ABOVE the reserved
        // superblock/checkpoint region. With base 0, segment 0's address range
        // covers the live checkpoint slots and a reclaim erase would destroy
        // them.
        segs: SegTable::with_base(data_base, num_segments),
        ckpt_seg_seq: 0,
        sched: Scheduler::new(sched_cfg),
        metrics: Metrics::default(),
        ckpt_buf,
        rng: slate_kv_core::index::XorShift64::new(rng_seed),
        scratch_buf: slate_kv_core::slate::ScratchWorkspace::new(),
    };

    println!("Slate mounted. Running test loop...");
    let mut i: u32 = 0;
    loop {
        let key = b"async_test_key";
        let val = b"async_test_value";

        // `append_hot` keeps `engine.records_in_epoch` and the sequence counter
        // in step; the raw `log_hot.append` used here before did not, so the Θ
        // epoch-seal trigger never fired.
        match slate.append_hot(slate_kv_core::config::OP_PUT, key, val) {
            Ok(offset) => {
                let _ = slate.index_update_offset(key, offset);
                i += 1;
                if i.is_multiple_of(100) {
                    println!("Appended {} records", i);
                }
            }
            Err(e) => {
                // Previously this printed forever at ~8100 records. Report the
                // state once and stop rather than filling the serial log with
                // thousands of identical lines.
                println!("Append error at record {}: {:?}", i, e);
                report(&slate, i);
                println!("halting");
                loop {
                    core::hint::spin_loop();
                }
            }
        }

        // Tell the scheduler an op happened and commit when it says to. The
        // previous code consulted `next_commit_deadline_ms` without ever calling
        // `sched.on_append`, so `ops_since_commit` stayed 0, the deadline was
        // always `None`, and nothing was ever committed: the 4 KiB batch buffer
        // filled after ~55 records and every later append returned `BatchFull`.
        // `now_ms` is a synthetic clock — one tick per record — which is enough
        // to exercise both the b_commit and deadline triggers without a timer
        // peripheral wired up.
        let now_ms = i as u64;
        if slate.sched.on_append(now_ms) {
            if let Err(e) = slate_kv_core::task::block_on(slate.commit_async()) {
                println!("Commit error at record {}: {:?}", i, e);
                report(&slate, i);
                println!("halting");
                loop {
                    core::hint::spin_loop();
                }
            }
        }

        if i.is_multiple_of(1000) {
            // Flush the batch first: `seal_epoch_now_async` checkpoints the
            // current `write_offset`, so an uncommitted batch would be replayed
            // as free space by the next mount.
            if let Err(e) = slate_kv_core::task::block_on(slate.commit_async()) {
                println!("Commit-before-seal error: {:?}", e);
            }
            if let Err(e) = slate_kv_core::task::block_on(slate.seal_epoch_now_async()) {
                println!("Seal error: {:?}", e);
            }
            if let Err(e) = slate_kv_core::task::block_on(slate.compact_async()) {
                println!("Compact error: {:?}", e);
            }
            report(&slate, i);
        }
    }
}

/// Prints the write-amplification buckets and segment census.
///
/// The previous loop printed only "Appended N records", so when it wedged the
/// serial log carried no state at all — just `Commit error: Io` forever. These
/// counters are what make the failure diagnosable from a log alone.
fn report<F, C, S>(slate: &Slate<F, C, S>, i: u32)
where
    F: slate_kv_hal::AsyncFlash,
    C: slate_kv_hal::AsyncMonotonicCounter,
    S: slate_kv_core::log::Sealer,
{
    let hot = slate.log_hot.head.write_offset;
    let cold = slate.log_cold.head.write_offset;
    let cap = slate_kv_hal::AsyncFlash::capacity(&slate.flash);
    println!(
        "[{}] epoch={} hot={} cold={} free_to_end={} segs(free={}/{} sealed={})",
        i,
        slate.engine.epoch,
        hot,
        cold,
        cap.saturating_sub(hot.max(cold)),
        slate.segs.free_count(),
        slate.segs.num_segments,
        slate
            .segs
            .count_in_state(slate_kv_core::gc::SegState::Sealed),
    );
    #[cfg(feature = "metrics")]
    {
        let m = &slate.metrics;
        let wa_bp = (m.flash_bytes() * 10_000) / m.user_bytes.max(1);
        println!(
            "     user={} gc={} parity={} marker={} ckpt={} erases={} WA={}.{:04}",
            m.user_bytes,
            m.gc_bytes,
            m.parity_bytes,
            m.marker_bytes,
            m.ckpt_bytes,
            m.erases,
            wa_bp / 10_000,
            wa_bp % 10_000
        );
        if m.gc_open_failed > 0 {
            println!(
                "     WARNING: gc could not decrypt {} records",
                m.gc_open_failed
            );
        }
    }
}
