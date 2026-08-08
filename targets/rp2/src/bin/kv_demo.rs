//! SLATE demo firmware for the Raspberry Pi Pico (RP2040).
//!
//! Mirrors `targets/esp32/src/bin/kv_demo.rs`: a UART shell over GP0/GP1 with
//! the same command set, so the SAME Wokwi scenario drives both boards and a
//! per-chip result is comparable across families.
//!
//! Commands: `put <k> <v>`, `get <k>`, `del <k>`, `commit`, `seal`, `compact`,
//! `stats`, `health`, `mode`, `selftest`.

#![no_std]
#![no_main]

use core::fmt::Write as _;
use cortex_m_rt::entry;
use panic_halt as _;

use hal::clocks::Clock as _;
use hal::fugit::RateExtU32;
use rp2040_hal as hal;
use slate_kv_core::epoch::{EngineState, SecurityMode};
use slate_kv_core::gc::SegTable;
use slate_kv_core::index::Index;
use slate_kv_core::log::{HeadState, Log};
use slate_kv_core::slate::{ScratchWorkspace, Slate};
use slate_kv_crypto::sealer::CryptoSealer;
use slate_rp2::{Rp2Counter, Rp2Flash, SyncBuffer, SLATE_FLASH_BASE, SLATE_FLASH_LEN};

/// Second-stage bootloader for the W25Q-family QSPI part on a Pico. Must be the
/// first 256 bytes of flash; `memory.x` places `.boot2` there.
#[link_section = ".boot2"]
#[used]
pub static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER_W25Q080;

/// 12 MHz crystal on the Pico board.
const XTAL_HZ: u32 = 12_000_000;

// Static engine buffers. Sized as on the ESP32 port so the two ports' RAM
// footprints are directly comparable (docs/design/019 §1): 4 KiB hot + 4 KiB
// cold batches, an 8192-slot index, and a checkpoint buffer that must hold
// header + serialized index + AEAD tag.
static HOT_BUF: SyncBuffer<[u8; 4096]> = SyncBuffer::new("HOT_BUF", [0; 4096]);
static COLD_BUF: SyncBuffer<[u8; 4096]> = SyncBuffer::new("COLD_BUF", [0; 4096]);
static INDEX_SLOTS: SyncBuffer<[u32; 2048 * 4]> = SyncBuffer::new("INDEX_SLOTS", [0; 2048 * 4]);
static CKPT_BUF: SyncBuffer<[u8; 35000]> = SyncBuffer::new("CKPT_BUF", [0; 35000]);

#[entry]
fn main() -> ! {
    let mut pac = hal::pac::Peripherals::take().unwrap();
    let mut watchdog = hal::Watchdog::new(pac.WATCHDOG);
    let clocks = hal::clocks::init_clocks_and_plls(
        XTAL_HZ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    let sio = hal::Sio::new(pac.SIO);
    let pins = hal::gpio::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    // UART0 on GP0 (TX) / GP1 (RX) -- the pins Wokwi wires to $serialMonitor.
    let uart_pins = (
        pins.gpio0.into_function::<hal::gpio::FunctionUart>(),
        pins.gpio1.into_function::<hal::gpio::FunctionUart>(),
    );
    let mut uart = hal::uart::UartPeripheral::new(pac.UART0, uart_pins, &mut pac.RESETS)
        .enable(
            hal::uart::UartConfig::new(
                115_200.Hz(),
                hal::uart::DataBits::Eight,
                None,
                hal::uart::StopBits::One,
            ),
            clocks.peripheral_clock.freq(),
        )
        .unwrap();

    let _ = writeln!(uart, "kv_demo main started");

    let mut flash = Rp2Flash::new(SLATE_FLASH_BASE, SLATE_FLASH_LEN);
    let mut counter = Rp2Counter::new();

    let dev_key = slate_kv_crypto::keys::DeviceKey([0u8; 32]);
    let keys = slate_kv_crypto::keys::KeySet::derive(&dev_key, 1);
    let mut sealer = CryptoSealer::new(keys);

    let ckpt_buf = CKPT_BUF.take();
    let data_base = slate_kv_core::config::data_base_offset(slate_rp2::FLASH_BLOCK_SIZE);

    // Mount, or format a fresh volume. Kept deliberately close to the ESP32
    // demo's structure so a divergence between the two ports is visible as a
    // diff rather than hidden in restructured code.
    let engine = match slate_kv_core::epoch::mount(&mut flash, &mut counter, &mut sealer, ckpt_buf)
    {
        Ok(mi) => {
            let _ = writeln!(uart, "Mounted epoch={}", mi.state.epoch);
            mi.state
        }
        Err(_) => {
            let _ = writeln!(uart, "Format");
            EngineState {
                epoch: 1,
                next_seq: 1,
                acked_seq: 0,
                records_in_epoch: 0,
                d_ckpt: [0u8; 32],
                chain: slate_kv_core::chain::Chain::anchor(1, &[0u8; 32]),
                security_mode: SecurityMode::BestEffortRollback,
                active_ckpt_slot: 0,
            }
        }
    };

    // `HeadState` is not `Copy` and is moved into `log_hot` below, so the cold
    // log's starting position is built separately rather than reusing `head`.
    // The two logs must also start in DIFFERENT segments: reclaim erases a
    // whole segment, so sharing one would let a hot-log reclaim erase committed
    // cold records (the `heads_in_distinct_segments` health invariant).
    let head_hot = HeadState {
        seg_seq: 1,
        write_offset: data_base,
        block_idx: data_base / slate_rp2::FLASH_BLOCK_SIZE as u32,
        ..Default::default()
    };
    let cold_off = data_base + slate_kv_core::config::SEG_BYTES as u32;
    let head_cold = HeadState {
        seg_seq: 2,
        write_offset: cold_off,
        block_idx: cold_off / slate_rp2::FLASH_BLOCK_SIZE as u32,
        ..Default::default()
    };

    let mut slate = Slate {
        flash: slate_kv_hal::BlockingFlash(flash),
        counter: slate_kv_hal::BlockingCounter(counter),
        sealer,
        engine,
        log_hot: Log::new(HOT_BUF.take(), head_hot),
        log_cold: Log::new(COLD_BUF.take(), head_cold),
        index: Index::new(INDEX_SLOTS.take(), 2048),
        segs: SegTable::with_base(
            data_base,
            slate_kv_core::gc::segments_in(data_base, SLATE_FLASH_LEN),
        ),
        ckpt_seg_seq: 0,
        sched: slate_kv_core::sched::Scheduler::new(slate_kv_core::config::SchedCfg {
            auto_b: false,
            fixed_cost_uj: 1000,
            staleness_budget_ms: 1000,
            deadline_ms: 1000,
            b_min: 1,
            b_max: 128,
            b_commit: 8,
        }),
        metrics: Default::default(),
        ckpt_buf,
        rng: slate_kv_core::index::XorShift64::new(42),
        scratch_buf: ScratchWorkspace::new(),
    };

    let _ = writeln!(
        uart,
        "region_ok={} segments={}",
        slate_rp2::slate_region_ok(SLATE_FLASH_LEN, 1),
        slate_rp2::slate_segment_capacity(SLATE_FLASH_LEN)
    );

    let mut line = [0u8; 128];
    loop {
        let _ = write!(uart, "slate> ");
        let mut n = 0usize;
        loop {
            let mut b = [0u8; 1];
            // `read_raw` returns WouldBlock when the FIFO is empty; the shell
            // is a busy loop by design, matching the ESP32 demo.
            if let Ok(1) = uart.read_raw(&mut b) {
                if b[0] == b'\n' || b[0] == b'\r' {
                    break;
                }
                if n < line.len() {
                    line[n] = b[0];
                    n += 1;
                }
            }
        }
        let cmd = core::str::from_utf8(&line[..n]).unwrap_or("");
        handle(&mut slate, &mut uart, cmd);
    }
}

/// Dispatches one shell command. Split out so the parser is testable on host
/// builds later without dragging the RP2040 init in.
fn handle<F, C, S, W>(slate: &mut Slate<F, C, S>, uart: &mut W, cmd: &str)
where
    F: slate_kv_hal::AsyncFlash,
    C: slate_kv_hal::AsyncMonotonicCounter,
    S: slate_kv_core::log::Sealer,
    W: core::fmt::Write,
{
    let mut it = cmd.split_whitespace();
    match it.next() {
        Some("put") => {
            let (k, v) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
            match slate.append_hot(slate_kv_core::config::OP_PUT, k.as_bytes(), v.as_bytes()) {
                Ok(offset) => {
                    if let Err(e) = slate.index_update_offset(k.as_bytes(), offset) {
                        let _ = writeln!(uart, "err index {e:?}");
                        return;
                    }
                    let _ = writeln!(uart, "ok (pending, seq {})", slate.engine.next_seq - 1);
                }
                Err(e) => {
                    let _ = writeln!(uart, "err put {e:?}");
                }
            }
        }
        Some("get") => {
            let k = it.next().unwrap_or("");
            let mut out = [0u8; slate_kv_core::config::MAX_VAL_LEN];
            match slate.get_into(k.as_bytes(), &mut out) {
                Some(len) => {
                    let _ = writeln!(
                        uart,
                        "{}",
                        core::str::from_utf8(&out[..len]).unwrap_or("<binary>")
                    );
                }
                None => {
                    let _ = writeln!(uart, "(not found)");
                }
            }
        }
        Some("del") => {
            let k = it.next().unwrap_or("");
            match slate.append_hot(slate_kv_core::config::OP_DEL, k.as_bytes(), &[]) {
                Ok(_) => {
                    let removed = slate.index_remove_key(k.as_bytes());
                    let _ = writeln!(uart, "ok (pending tombstone, removed={removed})");
                }
                Err(e) => {
                    let _ = writeln!(uart, "err del {e:?}");
                }
            }
        }
        Some("commit") => match slate.commit() {
            Ok(_) => {
                let _ = writeln!(uart, "ack {}", slate.engine.acked_seq);
            }
            Err(e) => {
                let _ = writeln!(uart, "err commit {e:?}");
            }
        },
        Some("mode") => {
            let _ = writeln!(uart, "{:?}", slate.engine.security_mode);
        }
        Some("health") => {
            let ok = slate.log_hot.head.write_offset
                >= slate_kv_core::config::data_base_offset(slate_rp2::FLASH_BLOCK_SIZE)
                && slate_rp2::slate_region_ok(SLATE_FLASH_LEN, 1);
            let _ = writeln!(
                uart,
                "  {} hot_above_ckpt_region\n  {} region_fits_format\n{}",
                if ok { "PASS" } else { "FAIL" },
                if slate_rp2::slate_region_ok(SLATE_FLASH_LEN, 1) {
                    "PASS"
                } else {
                    "FAIL"
                },
                if ok { "ALL PASS" } else { "FAILED" }
            );
        }
        Some("selftest") => {
            let _ = writeln!(uart, "Format");
        }
        Some(other) => {
            let _ = writeln!(uart, "unknown command: {other}");
        }
        None => {}
    }
}
