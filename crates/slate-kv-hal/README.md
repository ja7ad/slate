# slate-kv-hal

[![crates.io](https://img.shields.io/crates/v/slate-kv-hal.svg)](https://crates.io/crates/slate-kv-hal)
[![docs.rs](https://docs.rs/slate-kv-hal/badge.svg)](https://docs.rs/slate-kv-hal)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**The portability seam of [SLATE](https://github.com/ja7ad/slate).** Trait definitions only — flash, monotonic counter, entropy, clock. No implementations, no dependencies, no `unsafe`, no `std`.

```text
slate-kv (std)     slate-esp32 (bare metal)     slate-kv-sim (tests)
      └────────────────────┴─────────────────────────┘
                     implement these traits
                             │
                      slate-kv-hal
                             │
                      slate-kv-core  ← never touches hardware directly
```

`slate-kv-core` is written entirely against these traits. Porting SLATE to a new board or a new storage medium means implementing `Flash` and `MonotonicCounter` — nothing in the engine changes.

## Install

```sh
cargo add slate-kv-hal
```

```toml
[dependencies]
slate-kv-hal = "0.4"
```

The crate is `#![no_std]` and `#![forbid(unsafe_code)]` with **zero dependencies**, so adding it to a firmware build costs nothing.

## The traits

| Trait | Required by | Purpose |
|---|---|---|
| `Flash` | always | Page/block storage with NOR semantics |
| `MonotonicCounter` | always | Rollback protection anchor (G3) |
| `EntropySource` | key generation only | Random bytes; the engine needs none at runtime — nonces are `seq`-derived |
| `Clock` | optional | Coarse ms tick for the commit scheduler's deadline clamp |

### `Flash` — the contract you must honour

```rust
use slate_kv_hal::Flash;

pub struct MyFlash { /* ... */ }

impl Flash for MyFlash {
    type Error = MyError;

    fn page_size(&self) -> usize { 256 }        // program granularity
    fn block_size(&self) -> usize { 4096 }      // erase granularity, a multiple of page_size
    fn capacity(&self) -> u32 { 4 * 1024 * 1024 }

    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> { /* ... */ }
    fn program(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> { /* ... */ }
    fn erase(&mut self, block_addr: u32) -> Result<(), Self::Error> { /* ... */ }
}
```

The engine's durability proof rests on four properties of this implementation (report §2.1). Getting them wrong does not produce a compile error — it produces silent data loss, so read these carefully:

1. **Erased state is all-ones.** After `erase`, every byte in the block reads `0xFF`. The engine scans for `0xFF` to find the log head.
2. **Program-once-per-erase.** `program` may only write pages that are still in the erased state. Programming a non-erased page must return `Err`, not silently succeed.
3. **Alignment.** `addr` must be page-aligned and `buf.len()` a multiple of `page_size`; `block_addr` must be block-aligned. Reject anything else.
4. **Return means durable.** When `program` returns `Ok`, the bytes must survive an immediate power cut. On an OS-backed implementation this means an `fsync` (or `F_FULLFSYNC` on macOS) *before* returning — see [`slate-kv`'s `FileFlash`](https://github.com/ja7ad/slate/blob/main/crates/slate-kv/src/file_flash.rs). This is the property the acknowledgement rule (Theorem 4.1, prefix-durability) is built on.

Partial writes are allowed to be observed after a crash: the engine detects torn tails and truncates them. What it cannot recover from is a `program` that returns `Ok` before the data is stable.

### `MonotonicCounter` — and honest degradation

```rust
use slate_kv_hal::{CounterKind, MonotonicCounter};

impl MonotonicCounter for MyCounter {
    type Error = MyError;

    fn kind(&self) -> CounterKind { CounterKind::Hardware }
    fn read(&mut self) -> Result<u64, Self::Error> { /* ... */ }
    fn increment(&mut self) -> Result<u64, Self::Error> { /* durable before Ok */ }
}
```

`kind()` is not cosmetic — it is how the engine reports what it can actually guarantee:

| `CounterKind` | Backing | Resulting `SecurityMode` |
|---|---|---|
| `Hardware` | eFuse / RPMB / TPM — cannot be rolled back by an at-rest attacker | `Full` — epoch-granular rollback protection (G3) |
| `BestEffort` | An ordinary file or NVS entry an attacker with storage access could restore | `BestEffortRollback` |
| `None` | No counter available | `NoRollbackProtection` |

Report the truth here. A `BestEffort` counter reported as `Hardware` makes the engine claim a guarantee it does not have — the one failure mode SLATE explicitly refuses to have (invariant 7, "honest degradation").

`increment()` must be durable before returning `Ok`, and must return `Err` once its write budget is exhausted (eFuse counters are finite; the engine burns one per epoch, so budget × Θ bounds device lifetime).

## Existing implementations

Rather than starting from scratch, copy the one closest to your target:

| Implementation | Crate | Notes |
|---|---|---|
| `FileFlash` / `FileCounter` | [`slate-kv`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv) | Files on any OS; `BestEffort` counter |
| `EspFlash` / `EspCounter` | [`slate-esp32`](https://github.com/ja7ad/slate/tree/main/targets/esp32) | `esp-storage` partition; eFuse or flash-backed counter |
| `SimFlash` / `SimCounter` | [`slate-kv-sim`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv-sim) | In-RAM, with power-loss/bad-block/bit-rot injection |

## Testing your implementation

The fastest way to find contract violations is to run your backend through the simulator's property tests. A backend that passes `slate-kv-sim`'s crash Monte-Carlo (recovered prefix == acknowledged prefix, zero violations) is honouring the durability contract.

```sh
cargo test -p slate-kv-sim
```

The unit tests in this crate include a `StubFlash` that demonstrates each rule (all-ones erase, program-once rejection, alignment checks) in ~40 lines — a useful reference while writing your own.

## Report references

`Flash` semantics §2.1 · counter and adversary model §2.4 · counter protocol and honest degradation §3.4. See [`docs/SLATE_FORMAL_SPECIFICATION.md`](https://github.com/ja7ad/slate/blob/main/docs/SLATE_FORMAL_SPECIFICATION.md).

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
