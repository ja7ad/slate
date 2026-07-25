# slate-esp32

[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**The bare-metal reference port of [SLATE](https://github.com/ja7ad/slate).** `esp-hal` backends for the [HAL traits](https://github.com/ja7ad/slate/tree/main/crates/slate-kv-hal) plus demo and benchmark firmware — the build that demonstrates the whole point of the project: an encrypted, tamper-evident, crash-safe key-value store running on a microcontroller with no heap, no OS, and a fixed RAM budget.

Firmware, not a library. It is **excluded from the root Cargo workspace** because it builds for a RISC-V target with its own `.cargo/config.toml`; build it from this directory.

## Build

```sh
cd targets/esp32
cargo build --release --bin kv_demo \
  --no-default-features --features chip-esp32c3,counter-flash \
  --target riscv32imc-unknown-none-elf
```

Or from the repo root: `make build-esp`.

Flash to a device with [`espflash`](https://github.com/esp-rs/espflash):

```sh
cargo install espflash
espflash flash --monitor target/riscv32imc-unknown-none-elf/release/kv_demo
```

## Run it without hardware

```sh
./scripts/qemu_run.sh --fresh     # build, build a 4 MB image, boot it in QEMU
./scripts/qemu_crash.sh           # power-cut and recovery scenario
```

`qemu_run.sh` pads the flash image to 4 MB with **`0xFF`**, not zeros. This is not a detail you can skip: `0xFF` is the erased state of NOR flash, and `EspFlash::program` correctly refuses to write a page that is not erased. An image padded with zeros fails on the first write with `ProgramWithoutErase`, which looks like an engine bug and is not one.

There is also a [Wokwi](https://wokwi.com) setup in `wokwi/` for browser-based simulation, and `scripts/serial_drive.py` for driving the UART shell from a host.

## Features

Pick exactly one chip and exactly one counter backend — there is no default.

| Feature | Purpose |
|---|---|
| `chip-esp32c3` | Target chip (RISC-V). Wires through to `esp-hal`, `esp-backtrace`, `esp-println`, `esp-storage`, `esp-bootloader-esp-idf`. |
| `counter-efuse` | Monotonic counter in eFuse → `CounterKind::Hardware` |
| `counter-flash` | Counter in a reserved flash region → `CounterKind::BestEffort` |
| `counter-none` | No counter → `CounterKind::None` |
| `metrics` | Enables `slate-kv-core/metrics` counters for the energy bench |

Xtensa parts (esp32, esp32s3) are on the roadmap — the crate is structured for them, but only `chip-esp32c3` is wired up today.

### Choosing a counter backend

This choice *is* your rollback-protection story, and the engine reports the consequence honestly through `security_mode()`:

| Feature | `CounterKind` | `SecurityMode` | Trade-off |
|---|---|---|---|
| `counter-efuse` | `Hardware` | `Full` | Real epoch-granular rollback protection. eFuse writes are **irreversible and finite** — the budget divided by Θ bounds device lifetime. |
| `counter-flash` | `BestEffort` | `BestEffortRollback` | No fuse burn, no lifetime ceiling. An attacker who can rewrite flash can restore an old counter along with an old log. |
| `counter-none` | `None` | `NoRollbackProtection` | Tamper-evidence and confidentiality only. |

Confidentiality (G1) and tamper-evidence (G2) hold in all three. Only rollback protection (G3) varies, and the device says which mode it is in rather than claiming a guarantee it cannot deliver.

## Binaries

| Binary | What it does |
|---|---|
| `kv_demo` | Mounts the engine over static buffers and exposes put/get/delete over a UART shell. The clearest read of how a bare-metal host assembles the engine. |
| `slate_node` | Fuller node firmware: the demo plus GC and epoch handling. |
| `bench` | Throughput / boot-time / energy measurement firmware. Currently a skeleton — it boots and prints, but the measurement loop is not implemented. |

## The backends

`src/lib.rs` provides the three pieces a bare-metal host needs:

- **`EspFlash`** — implements `Flash` over an `esp-storage` partition, offset by a `base` address so the engine's addresses stay partition-relative. Before programming it verifies the target pages read `0xFF` and returns an error otherwise, upholding program-once-per-erase rather than trusting the caller.
- **`EspCounter`** — implements `MonotonicCounter` over eFuse or a reserved flash region at `0x300000`, chosen by feature. `with_kind` exists so a build can be honest about a degraded backend.
- **`SyncBuffer<T>`** — a `const`-constructible static cell whose `take()` hands out a `&'static mut T` exactly once, panicking on a second call. This is how the engine's buffers are provided without a heap and without `static mut`.

## RAM budget

The engine is heapless, so its working set is whatever this firmware statically allocates. `kv_demo` declares it explicitly:

```rust
static HOT_BUF:     SyncBuffer<[u8; 4096]>      = ...;   //  4 KB  hot batch
static COLD_BUF:    SyncBuffer<[u8; 4096]>      = ...;   //  4 KB  GC batch
static INDEX_SLOTS: SyncBuffer<[u32; 2048 * 4]> = ...;   // 32 KB  cuckoo index (~8k keys)
static CKPT_BUF:    SyncBuffer<[u8; 35000]>     = ...;   // 35 KB  checkpoint scratch
```

Tune these to your part before anything else — they are the entire memory cost of the store, they are visible in the map file, and they cannot grow at runtime. The index at 4 slots/bucket and 4 bytes/slot works out to roughly 4.5 bytes per live key.

The report §9 operating point for an ESP32-class device is B = 27, Θ = 16 384, f = 8 fingerprint bits, u = 0.5, RS m = 4 over k = 8 — the defaults in [`slate-kv-core::config`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv-core).

## Flash layout

The engine owns a partition; the counter (in `counter-flash` mode) lives at `0x300000`, deliberately above the data region at `[0x100000, 0x180000)`. If you re-partition, keep the counter region outside the log's arena — an overlap is silent until the log grows into it.

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
