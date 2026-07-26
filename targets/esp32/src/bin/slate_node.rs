#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::uart::Uart;
use esp_println::{print, println};

use slate_kv_core::config::{SchedCfg, OP_DEL, OP_PUT};
use slate_kv_core::gc::SegTable;
use slate_kv_core::index::Index;
use slate_kv_core::log::HeadState;
use slate_kv_core::metrics::Metrics;
use slate_kv_core::sched::Scheduler;
use slate_kv_core::slate::Slate;

use slate_kv_crypto::sealer::CryptoSealer;
use slate_esp32::{EspCounter, EspFlash, SyncBuffer};

esp_bootloader_esp_idf::esp_app_desc!();

static HOT_BUF: SyncBuffer<[u8; 4096]> = SyncBuffer::new("HOT_BUF", [0; 4096]);
static COLD_BUF: SyncBuffer<[u8; 4096]> = SyncBuffer::new("COLD_BUF", [0; 4096]);
static INDEX_SLOTS: SyncBuffer<[u32; 2048 * 4]> = SyncBuffer::new("INDEX_SLOTS", [0; 2048 * 4]);
static CKPT_BUF: SyncBuffer<[u8; 35000]> = SyncBuffer::new("CKPT_BUF", [0; 35000]);

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let mut uart = Uart::new(peripherals.UART0, esp_hal::uart::Config::default()).unwrap();

    let mut flash = EspFlash::new(0x100000, 4096 * 128, peripherals.FLASH);
    let mut counter = EspCounter::new();

    let dev_key = slate_kv_crypto::keys::DeviceKey([0x53u8; 32]); // Node master key
    let keys = slate_kv_crypto::keys::KeySet::derive(&dev_key, 1);
    let mut sealer = CryptoSealer::new(keys);

    let ckpt_buf = CKPT_BUF.take();
    let (engine_state, _plain_len) =
        match slate_kv_core::epoch::mount(&mut flash, &mut counter, &mut sealer, &mut *ckpt_buf) {
            Ok(mi) => (mi.state, mi.plain_len),
            Err(_) => {
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

    let sched_cfg = SchedCfg {
        auto_b: true,
        fixed_cost_uj: 400,
        staleness_budget_ms: 1000,
        deadline_ms: 1000,
        b_min: 1,
        b_max: 128,
        b_commit: 8,
    };

    let rng_seed = engine_state.epoch.max(1) ^ 42;
    let mut slate = Slate {
        flash: slate_kv_hal::BlockingFlash(flash),
        counter: slate_kv_hal::BlockingCounter(counter),
        sealer,
        engine: engine_state,
        log_hot: slate_kv_core::log::Log::new(
            HOT_BUF.take(),
            HeadState {
                seg_seq: 0,
                // Start the append log above the checkpoint region so commits
                // never program the live checkpoint pages.
                write_offset: slate_kv_core::config::data_base_offset(4096),
                block_idx: 0,
            },
        ),
        log_cold: slate_kv_core::log::Log::new(
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
        rng: slate_kv_core::index::XorShift64::new(rng_seed),
        scratch_buf: slate_kv_core::slate::ScratchWorkspace::new(),
    };

    println!("[SLATE ESP32 Storage Node Online]");
    print!("slate_node> ");

    let mut line_buf = [0u8; 128];
    let mut line_idx = 0;

    loop {
        let mut b = [0u8; 1];
        if let Ok(1) = uart.read(&mut b) {
            let ch = b[0];
            if ch == b'\n' || ch == b'\r' {
                if line_idx > 0 {
                    if let Ok(s) = core::str::from_utf8(&line_buf[..line_idx]) {
                        process_cmd(&mut slate, s);
                    }
                    line_idx = 0;
                }
                print!("slate_node> ");
            } else if line_idx < line_buf.len() {
                line_buf[line_idx] = ch;
                line_idx += 1;
            }
        }
        slate.sched.poll(0);
    }
}

fn process_cmd<F, C, S>(slate: &mut Slate<F, C, S>, cmd: &str)
where
    F: slate_kv_hal::AsyncFlash,
    C: slate_kv_hal::AsyncMonotonicCounter,
    S: slate_kv_core::log::Sealer,
{
    let parts: Vec<&str, 4> = parse_words(cmd);
    if parts.is_empty() {
        return;
    }

    match parts[0] {
        "put" => {
            if parts.len() < 3 || parts[1].as_bytes().is_empty() {
                println!("ERR invalid_args");
                return;
            }
            let key = parts[1].as_bytes();
            let val = parts[2].as_bytes();
            let seq = slate.engine.next_seq;
            if let Ok((_seq, offset)) = slate.log_hot.append(
                seq,
                slate.engine.epoch,
                OP_PUT,
                key,
                val,
                &mut slate.sealer,
                &mut slate.engine.chain,
            ) {
                slate.engine.next_seq += 1;
                let _ = slate.index_update_offset(key, offset);
                println!("OK seq={}", seq);
            } else {
                println!("ERR append_failed");
            }
        }
        "commit" => {
            if slate.commit().is_ok() {
                println!("ACK acked_seq={}", slate.engine.acked_seq);
            } else {
                println!("ERR commit_failed");
            }
        }
        "get" => {
            if parts.len() < 2 || parts[1].as_bytes().is_empty() {
                println!("ERR invalid_args");
                return;
            }
            let key = parts[1].as_bytes();
            let mut cbuf = slate_kv_core::index::CandidateBuf::new();
            slate.index.candidates(key, &mut cbuf);
            if cbuf.as_slice().is_empty() {
                println!("ERR not_found");
            } else {
                println!("OK candidate_count={}", cbuf.as_slice().len());
            }
        }
        "del" => {
            if parts.len() < 2 || parts[1].as_bytes().is_empty() {
                println!("ERR invalid_args");
                return;
            }
            let key = parts[1].as_bytes();
            let seq = slate.engine.next_seq;
            if let Ok(_) = slate.log_hot.append(
                seq,
                slate.engine.epoch,
                OP_DEL,
                key,
                &[],
                &mut slate.sealer,
                &mut slate.engine.chain,
            ) {
                slate.engine.next_seq += 1;
                slate.index_remove_key(key);
                println!("OK seq={}", seq);
            } else {
                println!("ERR del_failed");
            }
        }
        "ping" => {
            println!("pong");
        }
        _ => {
            println!("ERR unknown_command");
        }
    }
}

struct Vec<T, const N: usize> {
    data: [Option<T>; N],
    len: usize,
}

impl<T: Copy, const N: usize> Vec<T, N> {
    fn new() -> Self {
        Self {
            data: [None; N],
            len: 0,
        }
    }

    fn push(&mut self, val: T) {
        if self.len < N {
            self.data[self.len] = Some(val);
            self.len += 1;
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl<T: Copy, const N: usize> core::ops::Index<usize> for Vec<T, N> {
    type Output = T;
    fn index(&self, idx: usize) -> &Self::Output {
        self.data[idx].as_ref().unwrap()
    }
}

fn parse_words<'a, const N: usize>(s: &'a str) -> Vec<&'a str, N> {
    let mut res = Vec::new();
    for word in s.split_whitespace() {
        res.push(word);
    }
    res
}
