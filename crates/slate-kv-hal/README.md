# slate-kv-hal

[![crates.io](https://img.shields.io/crates/v/slate-kv-hal.svg)](https://crates.io/crates/slate-kv-hal)
[![docs.rs](https://docs.rs/slate-kv-hal/badge.svg)](https://docs.rs/slate-kv-hal)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**The portability seam of [SLATE](https://github.com/ja7ad/slate).** Trait definitions plus the blocking↔async bridge adapters — flash, monotonic counter, entropy, clock. No storage implementations of its own.

```text
slate-kv (std)     slate-esp32 (bare metal)     slate-kv-sim (tests)
      └────────────────────┴─────────────────────────┘
                     implement these traits
                             │
                      slate-kv-hal
                             │
                      slate-kv-core  ← never touches hardware directly
```

`slate-kv-core` is written entirely against these traits. Porting SLATE to a new board or storage medium means implementing `Flash` and `MonotonicCounter`; nothing in the engine changes.

Driver requirements are normative — see [`../../docs/specification.md`](../../docs/specification.md) §§ 2.1 and 5.1.

## Install

```sh
cargo add slate-kv-hal
```

```toml
[dependencies]
slate-kv-hal = "0.5"
```

`#![no_std]`, `#![forbid(unsafe_code)]`, and **no dependencies at all** by default, so adding it to a firmware build costs nothing.

### Features

| Feature                  | Default | Effect                                                                                                                                                                                                        |
|--------------------------|---------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `async`                  | off     | A marker feature. Nothing in this crate is gated on it — `AsyncFlash`, `AsyncMonotonicCounter` and the adapters are always available. It exists so `slate-kv-core`'s `async` feature has something to enable. |
| `embedded-storage-async` | off     | Implies `async` and pulls in `embedded-storage-async`, adding the `storage_async` module with `NorFlashAdapter`. This is the only feature that changes what compiles.                                         |

## The traits

| Trait                   | Required by         | Purpose                                                                                                          |
|-------------------------|---------------------|------------------------------------------------------------------------------------------------------------------|
| `Flash`                 | blocking hosts      | Page/block storage with NOR semantics                                                                            |
| `AsyncFlash`            | the engine          | What `slate-kv-core` actually calls; every method-for-method twin of `Flash` returning a future                  |
| `MonotonicCounter`      | blocking hosts      | Rollback-protection anchor                                                                                       |
| `AsyncMonotonicCounter` | the engine          | Async twin                                                                                                       |
| `EntropySource`         | key generation only | Random bytes. The engine needs none at runtime — nonces are derived from `seq`                                   |
| `Clock`                 | optional            | Coarse ms tick for the commit scheduler's deadline clamp. The engine never calls it; the host passes `now_ms` in |

### Which pair do I implement?

Implement the **blocking** pair, then wrap it:

```rust,ignore
use slate_kv_hal::{BlockingCounter, BlockingFlash};

// BlockingFlash<F: Flash> implements AsyncFlash by returning already-ready
// futures; BlockingCounter<C: MonotonicCounter> does the same for the counter.
// Both are newtypes with a public field, so `.0` gets your value back.
let flash = BlockingFlash(my_flash);
let counter = BlockingCounter(my_counter);
```

This is what [`slate-kv`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv) does, and it is what makes the engine's blocking façade free: those futures never return `Pending` for an I/O reason, so `block_on` completes without spinning. The adapters also implement the blocking traits themselves, so a `BlockingFlash<F>` is usable wherever a `Flash` is.

Implement `AsyncFlash` directly only when your driver genuinely suspends — a QSPI peripheral with a DMA completion interrupt, say. In that case use the engine's `_async` methods; driving a truly-suspending future through `block_on` busy-polls the CPU instead of sleeping.

`Flash` and `MonotonicCounter` are also implemented for `&mut F` / `&mut C`, so you can lend a backend to the engine without moving it. With the `embedded-storage-async` feature, `storage_async::NorFlashAdapter<T>` wraps any `embedded_storage_async::nor_flash::NorFlash` into an `AsyncFlash`.

### `Flash` — the contract you must honour

```rust,ignore
use slate_kv_hal::Flash;

impl Flash for MyFlash {
    type Error = MyError;                       // must be Debug

    fn page_size(&self) -> usize { 256 }        // program granularity
    fn block_size(&self) -> usize { 4096 }      // erase granularity, a multiple of page_size
    fn capacity(&self) -> u32 { 4 * 1024 * 1024 }

    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> { /* ... */ }
    fn program(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> { /* ... */ }
    fn erase(&mut self, block_addr: u32) -> Result<(), Self::Error> { /* ... */ }
}
```

Note `read` takes `&mut self`, so a shared-reference read path needs interior mutability on your side.

The engine's durability argument rests on four properties of your implementation. Getting them wrong produces no compile error — it produces silent data loss, so read these carefully:

1. **Erased state is all-ones.** After `erase`, every byte in the block reads `0xFF`. The engine scans for `0xFF` to find the log head, and treats the first erased page as the end of the log.
2. **Program-once-per-erase.** `program` may only write pages still in the erased state. Programming an already-programmed page must return `Err`, not silently succeed.
3. **Alignment.** `addr` must be page-aligned and `buf.len()` a multiple of `page_size`; `block_addr` must be block-aligned. Reject anything else rather than accommodating it.
4. **Return means durable.** When `program` returns `Ok`, the bytes must survive an immediate power cut. On an OS-backed implementation that means an `fsync` — `F_FULLFSYNC` on macOS — *before* returning; see [`slate-kv`'s `FileFlash`](https://github.com/ja7ad/slate/blob/main/crates/slate-kv/src/file_flash.rs). This is the property the acknowledgement rule is built on.

Partial writes may be observed after a crash: the engine detects torn tails and truncates them. What it cannot recover from is a `program` that returns `Ok` before the data is stable.

One capacity constraint from the engine side: index offsets are 24 bits (`OFF_BITS`), so `mount` rejects a volume reporting more than 16 MiB with `FormatError`.

`AsyncFlash` carries the identical contract, plus one addition: **`erase` is a single indivisible operation.** An implementation may await internally, but a caller must never observe a half-erased block, and dropping the returned future does not abort an erase already latched into the die.

### `MonotonicCounter` — and honest degradation

```rust,ignore
use slate_kv_hal::{CounterKind, MonotonicCounter};

impl MonotonicCounter for MyCounter {
    type Error = MyError;

    fn kind(&self) -> CounterKind { CounterKind::Hardware }
    fn read(&mut self) -> Result<u64, Self::Error> { /* ... */ }
    fn increment(&mut self) -> Result<u64, Self::Error> { /* NEW value; durable before Ok */ }
}
```

`kind()` is not cosmetic — it is how the engine decides what it can honestly guarantee:

| `CounterKind` | Backing                                                                     | Resulting `SecurityMode`                    |
|---------------|-----------------------------------------------------------------------------|---------------------------------------------|
| `Hardware`    | eFuse / RPMB / TPM — cannot be rolled back by an at-rest attacker           | `Full` — epoch-granular rollback protection |
| `BestEffort`  | An ordinary file or NVS entry an attacker with storage access could restore | `BestEffortRollback`                        |
| `None`        | No counter available; the engine skips the freshness check entirely         | `NoRollbackProtection`                      |

Report the truth here. A `BestEffort` counter reported as `Hardware` makes the engine claim a guarantee it does not have — the one failure mode SLATE explicitly refuses to have.

Even with `Hardware`, protection is **per epoch**: a rollback *within* the current epoch is not distinguished, and the window is bounded by `THETA`.

`increment()` must return the **new** value, must be durable before returning `Ok`, and must return `Err` once its write budget is exhausted. eFuse counters are finite and the engine burns one per epoch seal, so `budget × THETA` bounds device lifetime in operations — which is what `SlateConfig::validate` checks against `expected_life_ops`.

## Existing implementations

Rather than starting from scratch, copy the one closest to your target:

| Implementation              | Crate                                                                          | Notes                                                                 |
|-----------------------------|--------------------------------------------------------------------------------|-----------------------------------------------------------------------|
| `FileFlash` / `FileCounter` | [`slate-kv`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv)         | Files on any OS; `fsync`/`F_FULLFSYNC` barriers; `BestEffort` counter |
| `EspFlash` / `EspCounter`   | [`targets/esp32`](https://github.com/ja7ad/slate/tree/main/targets/esp32)      | `esp-storage` partition; eFuse or flash-backed counter                |
| `SimFlash` / `SimCounter`   | [`slate-kv-sim`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv-sim) | In-RAM, with power-loss and bad-block injection                       |

## Testing your implementation

The fastest way to find contract violations is to run your backend through the simulator's property tests. A backend that passes the crash Monte Carlo — recovered prefix equals acknowledged prefix, zero violations — is honouring the durability contract.

```sh
cargo test -p slate-kv-sim
```

This crate's own unit tests include a `StubFlash` and `StubCounter` demonstrating each rule (all-ones erase, program-once rejection, alignment checks, budget exhaustion) in a few dozen lines. They live in the `#[cfg(test)]` module, so they are a reading reference rather than something you can import.

## What async actually buys you, per platform

The engine layer is portable without qualification — `core::future` only, no allocator, no executor dependency, `#![forbid(unsafe_code)]` intact. What varies is **how much idle flash-bus time a platform can reclaim**, and that depends entirely on the board's flash driver:

| Platform                              | Flash driver reality                                                                                                                             | What async delivers                                                                                                                  |
|---------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------|
| **ESP32 / -C3 / -S3**                 | `esp-storage::FlashStorage` implements the **blocking** `embedded-storage` traits; internal-flash access additionally needs cache/interrupt care | **Concurrency only.** Wrap in `BlockingFlash`. Other tasks run between erases; no DMA offload and no CPU sleep during an erase.      |
| **STM32 / nRF52 + external QSPI NOR** | QSPI peripheral with DMA and a completion interrupt; async `embedded-storage-async` drivers exist                                                | **Full benefit.** Genuine DMA offload; the CPU can `WFI` during an erase. Implement `AsyncFlash` directly, or use `NorFlashAdapter`. |
| **RP2040 / RP2350 internal flash**    | Programming requires exiting XIP; code executes from flash, so the second core must be parked and interrupts handled carefully                   | **Restricted.** Async cannot make the XIP-disabled window interruptible; the erase span is a hard floor.                             |
| **Any board via the blocking façade** | n/a                                                                                                                                              | Behaviour identical to a purely synchronous engine. This is the fallback that makes the async interior non-breaking.                 |

Two caveats on the async story as shipped: the engine's **mount and tail-replay path is still on the blocking `Flash` trait**, so recovery is one uninterruptible span regardless of how good your async driver is; and there is no compile-time enforcement pairing blocking adapters with blocking methods — `Slate`'s fields are public, so the pairing is a convention you maintain. Both are recorded in [`../../docs/specification.md`](../../docs/specification.md) §§ 6.14 and 7.5.

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
