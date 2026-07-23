#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::uart::Uart;
use esp_println::{print, println};

use slate_core::config::SchedCfg;
use slate_core::gc::SegTable;
use slate_core::index::Index;
use slate_core::log::HeadState;
use slate_core::metrics::Metrics;
use slate_core::sched::Scheduler;
use slate_core::slate::Slate;

use slate_crypto::sealer::CryptoSealer;
use slate_esp32::{EspCounter, EspFlash, SyncBuffer};

esp_bootloader_esp_idf::esp_app_desc!();

static HOT_BUF: SyncBuffer<[u8; 4096]> = SyncBuffer::new([0; 4096]);
static COLD_BUF: SyncBuffer<[u8; 4096]> = SyncBuffer::new([0; 4096]);
static INDEX_SLOTS: SyncBuffer<[u32; 2048 * 4]> = SyncBuffer::new([0; 2048 * 4]);
static CKPT_BUF: SyncBuffer<[u8; 35000]> = SyncBuffer::new([0; 35000]);

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let mut uart = Uart::new(peripherals.UART0, esp_hal::uart::Config::default()).unwrap();

    let mut flash = EspFlash::new(0x100000, 4096 * 128, peripherals.FLASH);
    let mut counter = EspCounter::new();

    let dev_key = slate_crypto::keys::DeviceKey([0u8; 32]);
    let keys = slate_crypto::keys::KeySet::derive(&dev_key, 1);
    let mut sealer = CryptoSealer::new(keys);

    let mut mount_status = "OK";
    let ckpt_buf = CKPT_BUF.take();
    let (engine_state, _plain_len) =
        match slate_core::epoch::mount(&mut flash, &mut counter, &mut sealer, &mut *ckpt_buf) {
            Ok((st, len)) => (st, len),
            Err(slate_core::epoch::MountError::Tampered) => {
                mount_status = "Tampered";
                let st = slate_core::epoch::EngineState {
                    epoch: 1,
                    next_seq: 1,
                    acked_seq: 0,
                    d_ckpt: [0u8; 32],
                    chain: slate_core::chain::Chain::anchor(1, &[0u8; 32]),
                    records_in_epoch: 0,
                    security_mode: slate_core::epoch::SecurityMode::BestEffortRollback,
                    active_ckpt_slot: 0,
                };
                (st, 0)
            }
            Err(slate_core::epoch::MountError::Rollback) => {
                mount_status = "Rollback";
                let st = slate_core::epoch::EngineState {
                    epoch: 1,
                    next_seq: 1,
                    acked_seq: 0,
                    d_ckpt: [0u8; 32],
                    chain: slate_core::chain::Chain::anchor(1, &[0u8; 32]),
                    records_in_epoch: 0,
                    security_mode: slate_core::epoch::SecurityMode::BestEffortRollback,
                    active_ckpt_slot: 0,
                };
                (st, 0)
            }
            Err(_) => {
                mount_status = "Format";
                let st = slate_core::epoch::EngineState {
                    epoch: 1,
                    next_seq: 1,
                    acked_seq: 0,
                    d_ckpt: [0u8; 32],
                    chain: slate_core::chain::Chain::anchor(1, &[0u8; 32]),
                    records_in_epoch: 0,
                    security_mode: slate_core::epoch::SecurityMode::BestEffortRollback,
                    active_ckpt_slot: 0,
                };
                (st, 0)
            }
        };

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
        flash,
        counter,
        sealer,
        engine: engine_state,
        log_hot: slate_core::log::Log::new(
            HOT_BUF.take(),
            HeadState {
                seg_seq: 0,
                write_offset: 0,
                block_idx: 0,
            },
        ),
        log_cold: slate_core::log::Log::new(
            COLD_BUF.take(),
            HeadState {
                seg_seq: 0,
                write_offset: 0,
                block_idx: 0,
            },
        ),
        index: Index::new(INDEX_SLOTS.take(), 2048),
        segs: SegTable::new(128),
        ckpt_seg_seq: 0,
        sched: Scheduler::new(sched_cfg),
        metrics: Metrics::default(),
        ckpt_buf,
        rng: slate_core::index::XorShift64::new(rng_seed),
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
    F: slate_hal::Flash,
    C: slate_hal::MonotonicCounter,
    S: slate_core::log::Sealer,
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

            let seq = slate.engine.next_seq;
            match slate.log_hot.append(
                seq,
                slate_core::config::OP_PUT,
                k,
                v,
                &mut slate.sealer,
                &mut slate.engine.chain,
            ) {
                Ok((_, offset)) => {
                    slate.engine.next_seq += 1;
                    slate.index_update_offset(k, offset);
                    // Do not ack here, ack on commit!
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
            let mut cbuf = slate_core::index::CandidateBuf::new();
            slate.index.candidates(k, &mut cbuf);
            let mut found = false;
            for &off in cbuf.as_slice() {
                let mut hdr_bytes = [0u8; slate_core::config::REC_HDR_LEN];
                if slate.flash.read(off, &mut hdr_bytes).is_err() {
                    continue;
                }
                if let Ok(hdr) = slate_core::record::RecordHeader::decode(&hdr_bytes) {
                    if hdr.klen as usize == k.len() {
                        let total_len = 44 + hdr.klen as usize + hdr.vlen as usize;
                        let mut rec_bytes = [0u8; 44 + 256 + 1024];
                        if slate.flash.read(off, &mut rec_bytes[..total_len]).is_ok() {
                            let mut scratch = [0u8; 256 + 1024];
                            if slate
                                .sealer
                                .open_record(
                                    &hdr_bytes,
                                    &rec_bytes[slate_core::config::REC_HDR_LEN..total_len],
                                    &mut scratch,
                                )
                                .is_ok()
                                && &scratch[..hdr.klen as usize] == k
                            {
                                if let Ok(v_str) = core::str::from_utf8(
                                    &scratch
                                        [hdr.klen as usize..hdr.klen as usize + hdr.vlen as usize],
                                ) {
                                    println!("{}", v_str);
                                } else {
                                    print!("[binary data] ");
                                    for &b in &scratch[hdr.klen as usize..hdr.klen as usize + hdr.vlen as usize] {
                                        print!("{:02x}", b);
                                    }
                                    println!();
                                }
                                found = true;
                                break;
                            }
                        }
                    }
                }
            }
            if !found {
                println!("(not found)");
            }
        }
        Some("del") => {
            let k_opt = parts.next();
            if k_opt.is_none() || k_opt.unwrap().is_empty() {
                println!("ERR invalid_args");
                return;
            }
            let k = k_opt.unwrap().as_bytes();
            let seq = slate.engine.next_seq;
            if slate
                .log_hot
                .append(
                    seq,
                    slate_core::config::OP_DEL,
                    k,
                    &[],
                    &mut slate.sealer,
                    &mut slate.engine.chain,
                )
                .is_ok()
            {
                slate.engine.next_seq += 1;
                slate.index.remove(k, |_| true);
            }
        }
        Some("commit") => {
            match slate.commit() {
                Ok(_) => {
                    let ack_seq = slate.engine.acked_seq;
                    println!("ack {}", ack_seq);
                }
                Err(e) => {
                    println!("err commit {:?}", e);
                }
            }
        }
        Some("seal") => {
            // Force an epoch seal by pretending we reached THETA
            slate.engine.records_in_epoch = slate_core::config::THETA;
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
            #[cfg(feature = "metrics")]
            {
                let m = &slate.metrics;
                println!("stats: commits={} wakes={} user_bytes={} gc_bytes={} parity_bytes={} ckpt_bytes={} erases={}",
                    m.commits, m.wakes, m.user_bytes, m.gc_bytes, m.parity_bytes, m.ckpt_bytes, m.erases);
            }
            #[cfg(not(feature = "metrics"))]
            {
                println!("stats: commits=0 wakes=0");
            }
        }
        Some("mode") => match slate.counter.kind() {
            slate_hal::CounterKind::BestEffort => println!("BestEffortRollback"),
            slate_hal::CounterKind::Hardware => println!("Hardware"),
            slate_hal::CounterKind::None => println!("None"),
        },
        Some("selftest") => {
            println!("{}", mount_status);
        }
        Some("format") => {
            println!("err");
        }
        _ => {
            println!("unknown command");
        }
    }
}
