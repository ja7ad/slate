#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::uart::Uart;
use esp_println::{print, println};

use slate_kv_core::config::SchedCfg;
use slate_kv_core::gc::SegTable;
use slate_kv_core::index::Index;
use slate_kv_core::log::HeadState;
use slate_kv_core::metrics::Metrics;
use slate_kv_core::sched::Scheduler;
use slate_kv_core::slate::Slate;

use slate_esp32::{EspCounter, EspFlash, SyncBuffer};
use slate_kv_crypto::sealer::CryptoSealer;

esp_bootloader_esp_idf::esp_app_desc!();

static HOT_BUF: SyncBuffer<[u8; 4096]> = SyncBuffer::new("HOT_BUF", [0; 4096]);
static COLD_BUF: SyncBuffer<[u8; 4096]> = SyncBuffer::new("COLD_BUF", [0; 4096]);
static INDEX_SLOTS: SyncBuffer<[u32; 2048 * 4]> = SyncBuffer::new("INDEX_SLOTS", [0; 2048 * 4]);
static CKPT_BUF: SyncBuffer<[u8; 35000]> = SyncBuffer::new("CKPT_BUF", [0; 35000]);

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let mut uart = Uart::new(peripherals.UART0, esp_hal::uart::Config::default()).unwrap();
    println!("kv_demo main started");

    let mut flash = EspFlash::new(
        slate_esp32::SLATE_FLASH_BASE,
        slate_esp32::SLATE_FLASH_LEN,
        peripherals.FLASH,
    );
    let mut counter = EspCounter::new();

    let dev_key = slate_kv_crypto::keys::DeviceKey([0u8; 32]);
    let keys = slate_kv_crypto::keys::KeySet::derive(&dev_key, 1);
    let mut sealer = CryptoSealer::new(keys);

    let mut head_state = HeadState {
        seg_seq: 0,
        write_offset: 0,
        block_idx: 0,
            ..Default::default()
    };

    let mut mount_status = "OK";
    let ckpt_buf = CKPT_BUF.take();
    let (engine_state, _plain_len) =
        match slate_kv_core::epoch::mount(&mut flash, &mut counter, &mut sealer, &mut *ckpt_buf) {
            Ok(mi) => {
                // Resume the head where the checkpoint left it, so new records
                // append after the durable ones instead of over them.
                head_state.seg_seq = mi.ckpt_seg_seq;
                head_state.write_offset = mi.ckpt_write_offset;
                head_state.block_idx = mi.ckpt_write_offset / 4096;
                (mi.state, mi.plain_len)
            }
            Err(slate_kv_core::epoch::MountError::Tampered) => {
                mount_status = "Tampered";
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
            Err(slate_kv_core::epoch::MountError::Rollback) => {
                mount_status = "Rollback";
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
            Err(_) => {
                mount_status = "Format";
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

    // The append log must never overlap the checkpoint region (blocks 2..22),
    // whether we mounted a checkpoint that predates this rule or formatted fresh.
    let data_base = slate_kv_core::config::data_base_offset(4096);
    if head_state.write_offset < data_base {
        head_state.write_offset = data_base;
        head_state.block_idx = data_base / 4096;
    }

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
        flash: slate_kv_hal::BlockingFlash(flash),
        counter: slate_kv_hal::BlockingCounter(counter),
        sealer,
        engine: engine_state,
        log_hot: slate_kv_core::log::Log::new(HOT_BUF.take(), head_state),
        log_cold: slate_kv_core::log::Log::new(
            COLD_BUF.take(),
            HeadState {
                seg_seq: 1,
                // The cold log carries GC re-appends and tombstones and is
                // committed unconditionally alongside the hot log, so it must
                // start above the checkpoint region too. Leaving it at 0
                // programmed the live checkpoint pages on the first `del`.
                //
                // It must also start in a DIFFERENT segment from the hot log:
                // reclaim erases a whole segment, so two logs sharing one means
                // reclaiming it destroys the other log's records. Starting both
                // at `cold_write_offset` put both in segment 0 — which is what
                // the `heads_in_distinct_segments` health check reported, and
                // it matches this head's own `seg_seq: 1`.
                write_offset: cold_write_offset + slate_kv_core::config::SEG_BYTES as u32,
                block_idx: cold_block_idx,
            ..Default::default()
            },
        ),
        index: Index::new(INDEX_SLOTS.take(), 2048),
        // `with_base`, not `new`: segments tile the log area ABOVE the reserved
        // superblock/checkpoint region. With base 0, segment 0's address range
        // covers the live checkpoint slots and a reclaim erase would destroy
        // them.
        segs: SegTable::with_base(
            data_base,
            slate_esp32::slate_segment_capacity(slate_esp32::SLATE_FLASH_LEN),
        ),
        ckpt_seg_seq: 0,
        sched: Scheduler::new(sched_cfg),
        metrics: Metrics::default(),
        ckpt_buf,
        rng: slate_kv_core::index::XorShift64::new(rng_seed),
        scratch_buf: slate_kv_core::slate::ScratchWorkspace::new(),
    };

    println!("slate> ");

    let mut line_buf = [0u8; 128];
    let mut line_idx = 0;

    loop {
        let mut b = [0u8; 1];
        if let Ok(1) = uart.read(&mut b) {
            let ch = b[0];
            if ch == b'\n' || ch == b'\r' {
                if line_idx > 0 {
                    if let Ok(s) = core::str::from_utf8(&line_buf[..line_idx]) {
                        handle_cmd(&mut slate, s, mount_status);
                    }
                    line_idx = 0;
                }
                print!("slate> ");
            } else if line_idx < line_buf.len() {
                line_buf[line_idx] = ch;
                line_idx += 1;
            }
        }
        slate.sched.poll(0); // Poll scheduler
    }
}

fn handle_cmd<F, C, S>(slate: &mut Slate<F, C, S>, cmd: &str, mount_status: &str)
where
    F: slate_kv_hal::AsyncFlash,
    C: slate_kv_hal::AsyncMonotonicCounter,
    S: slate_kv_core::log::Sealer,
{
    let mut parts = cmd.split_whitespace();
    match parts.next() {
        Some("put") => {
            let k_opt = parts.next();
            let v_opt = parts.next();
            if k_opt.is_none() || k_opt.unwrap().is_empty() {
                println!("ERR invalid_args");
                return;
            }
            let k = k_opt.unwrap().as_bytes();
            let v = v_opt.unwrap_or("").as_bytes();

            // Go through `append_hot`, not `log_hot.append`: only the former
            // advances `records_in_epoch`, which drives the Theta epoch-seal
            // trigger. A raw append silently disables checkpointing and GC.
            match slate.append_hot(slate_kv_core::config::OP_PUT, k, v) {
                Ok(offset) => {
                    if let Err(e) = slate.index_update_offset(k, offset) {
                        println!("err index {:?}", e);
                        return;
                    }
                    println!("ok (pending, seq {})", slate.engine.next_seq - 1);
                    // Let the scheduler decide whether this append should
                    // trigger a commit, exactly as a real application would.
                    if slate.sched.on_append(0) {
                        match slate.commit() {
                            Ok(()) => println!("auto-commit ack {}", slate.engine.acked_seq),
                            Err(e) => println!("err commit {:?}", e),
                        }
                    }
                }
                Err(e) => {
                    println!("err put {:?}", e);
                }
            }
        }
        Some("get") => {
            let k_opt = parts.next();
            if k_opt.is_none() || k_opt.unwrap().is_empty() {
                println!("ERR invalid_args");
                return;
            }
            let k = k_opt.unwrap().as_bytes();
            // `get_into` resolves the record wherever it currently lives: the
            // uncommitted hot batch, the uncommitted cold batch, or flash. The
            // previous code read `flash` directly at the index offset, which for
            // an uncommitted record is a *future* address that still reads as
            // erased 0xFF — so `put k v` followed by `get k` always printed
            // "(not found)" until a commit happened to intervene.
            let mut val_buf = [0u8; slate_kv_core::config::MAX_VAL_LEN];
            match slate.get_into(k, &mut val_buf) {
                Some(n) => match core::str::from_utf8(&val_buf[..n]) {
                    Ok(v) => println!("{}", v),
                    Err(_) => {
                        print!("[binary data] ");
                        for &b in &val_buf[..n] {
                            print!("{:02x}", b);
                        }
                        println!();
                    }
                },
                None => println!("(not found)"),
            }
        }
        Some("del") => {
            let k_opt = parts.next();
            if k_opt.is_none() || k_opt.unwrap().is_empty() {
                println!("ERR invalid_args");
                return;
            }
            let k = k_opt.unwrap().as_bytes();
            match slate.append_hot(slate_kv_core::config::OP_DEL, k, &[]) {
                Ok(_) => {
                    // Match on the FULL key: `index.remove(k, |_| true)` accepted
                    // the first fingerprint match, so a colliding fingerprint
                    // could evict a different live key's slot.
                    let removed = slate.index_remove_key(k);
                    println!("ok (pending tombstone, removed={})", removed);
                    if slate.sched.on_append(0) {
                        match slate.commit() {
                            Ok(()) => println!("auto-commit ack {}", slate.engine.acked_seq),
                            Err(e) => println!("err commit {:?}", e),
                        }
                    }
                }
                Err(e) => println!("err del {:?}", e),
            }
        }
        Some("commit") => match slate.commit() {
            Ok(_) => {
                let ack_seq = slate.engine.acked_seq;
                println!("ack {}", ack_seq);
            }
            Err(e) => {
                println!("err commit {:?}", e);
            }
        },
        Some("seal") => {
            // Force an epoch seal by pretending we reached THETA
            slate.engine.records_in_epoch = slate_kv_core::config::THETA;
            match slate.commit() {
                Ok(_) => {
                    println!("OK");
                }
                Err(e) => {
                    println!("err seal {:?}", e);
                }
            }
        }
        Some("stats") => {
            let data_base = slate_kv_core::config::data_base_offset(4096);
            let cap = slate_kv_hal::AsyncFlash::capacity(&slate.flash);
            let hot = slate.log_hot.head.write_offset;
            let cold = slate.log_cold.head.write_offset;
            let seg_end = slate.segs.end_addr();

            println!("engine:");
            println!(
                "  epoch={} next_seq={} acked_seq={} records_in_epoch={}/{}",
                slate.engine.epoch,
                slate.engine.next_seq,
                slate.engine.acked_seq,
                slate.engine.records_in_epoch,
                slate_kv_core::config::THETA
            );
            println!("  security_mode={:?}", slate.engine.security_mode);

            println!("log:");
            println!(
                "  hot  head={} (+{} past data_base, {} B to region end)",
                hot,
                hot.saturating_sub(data_base),
                cap.saturating_sub(hot)
            );
            println!(
                "  cold head={} (+{} past data_base, {} B to region end)",
                cold,
                cold.saturating_sub(data_base),
                cap.saturating_sub(cold)
            );
            println!(
                "  batch hot={} B cold={} B",
                slate.log_hot.batch.data().len(),
                slate.log_cold.batch.data().len()
            );

            println!("index:");
            println!(
                "  keys={} slots={} load={}%",
                slate.index.len(),
                2048 * 4,
                (slate.index.len() * 100) / (2048 * 4)
            );

            println!("segments:");
            println!(
                "  total={} free={} sealed={} data_base={} seg_end={} cap={}",
                slate.segs.num_segments,
                slate.segs.free_count(),
                slate
                    .segs
                    .count_in_state(slate_kv_core::gc::SegState::Sealed),
                data_base,
                seg_end,
                cap
            );
            println!(
                "  ckpt_seg_seq={} cur_seg_seq={} (a sealed segment is reclaimable below ckpt_seg_seq)",
                slate.ckpt_seg_seq,
                slate.segs.current_seg_seq()
            );

            #[cfg(feature = "metrics")]
            {
                let m = &slate.metrics;
                println!("flash bytes written (write-amplification buckets):");
                println!("  user   ={}", m.user_bytes);
                println!("  gc     ={}", m.gc_bytes);
                println!("  parity ={}", m.parity_bytes);
                println!("  marker ={}", m.marker_bytes);
                println!("  ckpt   ={}", m.ckpt_bytes);
                println!("  total  ={}", m.flash_bytes());
                match m.write_amplification() {
                    // Printed in basis points: no FPU on the C3, and formatting
                    // an f32 would pull in a large soft-float formatter.
                    Some(_) => {
                        let wa_bp = (m.flash_bytes() * 10_000) / m.user_bytes.max(1);
                        println!("  WA     ={}.{:04} ", wa_bp / 10_000, wa_bp % 10_000);
                    }
                    None => println!("  WA     =unmeasured (no user bytes yet)"),
                }
                println!("durability:");
                println!(
                    "  commits={} wakes={} erases={}",
                    m.commits, m.wakes, m.erases
                );
                println!("gc:");
                println!(
                    "  scanned={} relocated={} open_failed={} segments_freed={}",
                    m.gc_scanned, m.gc_relocated, m.gc_open_failed, m.gc_segments_freed
                );
                if m.gc_open_failed > 0 {
                    println!("  WARNING: gc could not decrypt records it treated as garbage");
                }
            }
            #[cfg(not(feature = "metrics"))]
            {
                println!("metrics: DISABLED (rebuild with --features metrics)");
            }
        }
        Some("health") => {
            // Each check below corresponds to a failure mode that previously
            // surfaced only as an opaque `Io` on some later commit.
            let data_base = slate_kv_core::config::data_base_offset(4096);
            let cap = slate_kv_hal::AsyncFlash::capacity(&slate.flash);
            let hot = slate.log_hot.head.write_offset;
            let cold = slate.log_cold.head.write_offset;
            let mut fails = 0;

            // Not `mut`: the closure only prints. `fails` is incremented by the
            // caller at each site, so nothing is captured mutably here.
            let check = |name: &str, ok: bool, detail: &str| {
                if ok {
                    println!("  PASS {} {}", name, detail);
                } else {
                    println!("  FAIL {} {}", name, detail);
                }
            };

            println!("health:");
            let ok = hot >= data_base;
            if !ok {
                fails += 1;
            }
            check(
                "hot_above_ckpt_region",
                ok,
                "hot head must not overlap checkpoint slots",
            );

            let ok = cold >= data_base;
            if !ok {
                fails += 1;
            }
            check(
                "cold_above_ckpt_region",
                ok,
                "cold head must not overlap checkpoint slots",
            );

            let hot_seg = slate.segs.seg_of(hot);
            let cold_seg = slate.segs.seg_of(cold);
            let ok = hot_seg.is_none() || hot_seg != cold_seg;
            if !ok {
                fails += 1;
            }
            check(
                "heads_in_distinct_segments",
                ok,
                "reclaim erases a whole segment, so the two logs must not share one",
            );

            let ok = hot < cap && cold < cap;
            if !ok {
                fails += 1;
            }
            check(
                "heads_in_region",
                ok,
                "both heads must lie inside the mapped region",
            );

            let ok = slate_esp32::slate_region_ok(slate_esp32::SLATE_FLASH_LEN, 1);
            if !ok {
                fails += 1;
            }
            check(
                "region_fits_format",
                ok,
                "region must hold the reserved layout + 1 segment",
            );

            let ok = slate.segs.num_segments > 0;
            if !ok {
                fails += 1;
            }
            check(
                "segment_map_present",
                ok,
                "a zero-length segment map disables GC entirely",
            );

            #[cfg(feature = "metrics")]
            {
                let ok = slate.metrics.gc_open_failed == 0;
                if !ok {
                    fails += 1;
                }
                check(
                    "gc_never_discarded_unread",
                    ok,
                    "gc must not treat undecryptable records as garbage",
                );
            }

            if fails == 0 {
                println!("  ALL PASS");
            } else {
                println!("  {} CHECK(S) FAILED", fails);
            }
        }
        Some("mode") => match slate.counter.kind() {
            slate_kv_hal::CounterKind::BestEffort => println!("BestEffortRollback"),
            slate_kv_hal::CounterKind::Hardware => println!("Hardware"),
            slate_kv_hal::CounterKind::None => println!("None"),
        },
        Some("selftest") => {
            println!("{}", mount_status);
        }
        Some("format") => {
            println!("err");
        }
        Some("help") => {
            println!("commands: put <k> <v> | get <k> | del <k> | commit | seal | compact");
            println!("          stats | health | mode | selftest | help");
        }
        Some("compact") => match slate.compact() {
            Ok(()) => println!("OK"),
            Err(e) => println!("err compact {:?}", e),
        },
        _ => {
            println!("unknown command (try `help`)");
        }
    }
}
