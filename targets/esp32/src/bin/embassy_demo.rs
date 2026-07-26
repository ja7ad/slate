#![no_std]
#![no_main]

// use embassy_executor::Spawner;
// use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::uart::Uart;
use esp_println::println;
use core::sync::atomic::AtomicU32;

use slate_kv_core::config::SchedCfg;
use slate_kv_core::gc::SegTable;
use slate_kv_core::index::Index;
use slate_kv_core::log::HeadState;
use slate_kv_core::metrics::Metrics;
use slate_kv_core::sched::Scheduler;
use slate_kv_core::slate::Slate;

use slate_kv_crypto::sealer::CryptoSealer;
use slate_esp32::{EspCounter, EspFlash, SyncBuffer};
use slate_kv_hal::{BlockingFlash, BlockingCounter};

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

    let flash = EspFlash::new(0x100000, 4096 * 128, peripherals.FLASH);
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
        match slate_kv_core::task::block_on(slate_kv_core::epoch::mount_async(&mut async_flash, &mut async_counter, &mut sealer, &mut *ckpt_buf)) {
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

    let rng_seed = engine_state.epoch.max(1) ^ 42;
    let mut slate = Slate {
        flash: async_flash,
        counter: async_counter,
        sealer,
        engine: engine_state,
        log_hot: slate_kv_core::log::Log::new(
            hot_buf,
            head_state,
        ),
        log_cold: slate_kv_core::log::Log::new(
            cold_buf,
            HeadState {
                seg_seq: 0,
                write_offset: 0,
                block_idx: 0,
            },
        ),
        index: Index::new(index_slots, 2048),
        segs: SegTable::new(128),
        ckpt_seg_seq: 0,
        sched: Scheduler::new(sched_cfg),
        metrics: Metrics::default(),
        ckpt_buf,
        rng: slate_kv_core::index::XorShift64::new(rng_seed),
        scratch_buf: slate_kv_core::slate::ScratchWorkspace::new(),
    };

    println!("Slate mounted. Running test loop...");
    let mut i = 0;
    loop {
        let key = b"async_test_key";
        let val = b"async_test_value";
        let seq = slate.engine.next_seq;
        match slate.log_hot.append(
            seq,
            slate.engine.epoch,
            slate_kv_core::config::OP_PUT,
            key,
            val,
            &mut slate.sealer,
            &mut slate.engine.chain,
        ) {
            Ok((_, offset)) => {
                slate.engine.next_seq += 1;
                let _ = slate.index_update_offset(key, offset);
                i += 1;
                if i % 100 == 0 {
                    println!("Appended {} records", i);
                }
            }
            Err(e) => {
                println!("Append error: {:?}", e);
            }
        }
        
        if let Some(_deadline) = slate.next_commit_deadline_ms(i as u64) {
            slate_kv_core::task::block_on(slate.commit_async()).unwrap();
        } else {
            // Wait
        }
        
        if i % 1000 == 0 {
            slate_kv_core::task::block_on(slate.seal_epoch_now_async()).unwrap();
            slate_kv_core::task::block_on(slate.compact_async()).unwrap();
        }
    }
}
