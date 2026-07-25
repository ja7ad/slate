# slate-kv-sim

[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Where [SLATE](https://github.com/ja7ad/slate)'s paper claims become tests.** A deterministic in-RAM flash with fault injection — power loss at an arbitrary byte, bad blocks, program-once violations — plus the property tests that run the engine against it.

Development-only. Not published to crates.io and has no stable API; it exists so that "prefix-durability holds under arbitrary power loss" is a thing CI checks rather than a thing the report asserts.

## Run it

```sh
cargo test -p slate-kv-sim                       # everything
cargo test -p slate-kv-sim --test rs_recovery    # one suite
```

## Why simulate flash at all

Crash-safety bugs are the ones you cannot find by testing on real hardware: they need a power cut at one specific byte of one specific program operation, and you cannot arrange that on demand. `SimFlash` makes the crash point a parameter.

```rust
use slate_kv_sim::{Crash, SimFlash};

let mut flash = SimFlash::new(1024 * 1024, 256, 4096);

// Cut power partway through the 42nd program operation, 17 bytes in.
flash.power.crash = Crash::AtByte { op_index: 42, byte_in_op: 17 };

// Every subsequent program past that point returns SimFlashError::PowerLoss,
// leaving the medium in exactly the torn state real hardware would.
```

Then reopen the engine over the same `flash.mem` and assert the recovered state equals the acknowledged prefix. Nothing more, nothing less — no acknowledged write lost, no uncommitted write resurrected.

## What `SimFlash` models

Beyond the [`Flash`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv-hal) contract, it enforces the parts real NOR flash enforces and file-backed emulation quietly forgives:

| Behaviour | Modelled as |
|---|---|
| Program-once-per-erase | Per-page `programmed` flags; a second program returns `AlreadyProgrammed` |
| Erased state is all-ones | `erase` fills the block with `0xFF` |
| Alignment | Unaligned address or length returns `Unaligned` |
| Worn-out blocks | `bad_blocks: BTreeSet<u32>` — inserted blocks fail on program/erase |
| Power loss | `Crash::AtByte { op_index, byte_in_op }`, byte-exact |

Because `mem` is a plain `Vec<u8>`, a test can also corrupt bytes directly to simulate bit rot, or snapshot and restore an image to simulate a rollback attack.

It also counts what the energy model needs: `stats.programs`, `erases`, `bytes_programmed`, `user_bytes`, `gc_bytes`, `wakes`. Setting `is_gc_write` attributes the next writes to compaction, which is how write amplification is measured rather than estimated.

`SimCounter` completes the pair: a `MonotonicCounter` with a configurable budget, so counter exhaustion (`CounterExhausted`) is testable without burning eFuses.

## Test suites

| Test | Asserts |
|---|---|
| `crash_monte_carlo.rs` | Randomised crash points; recovered prefix == acknowledged prefix |
| `gc_compaction.rs` | `test_no_resurrection_prop3_1` — a deleted key never comes back after compaction (Invariant T); `test_wa_accounting` — write amplification stays within `1/(1-u)` |
| `rs_recovery.rs` | A database survives injected bad blocks via RS(12,8) reconstruction |

`debug.rs` is a scratch binary for manual poking, not an assertion suite.

## Energy accounting

```rust
use slate_kv_sim::power::{report, PowerModel, Stats};

let r = report(&stats, &PowerModel { /* per-byte, per-erase, per-wake costs */ });
```

Turns the flash counters into joules under the report §8 cost model, so a change that quietly doubles the number of flash wake-ups shows up as a number.

## Status: the `src/bin/` binaries are stubs

`crash_mc`, `wa_study` and `energy_check` are **placeholders**. They print plausibly shaped output from closed-form formulas without driving the engine — `wa_study` computes `1/(1-u)` directly rather than measuring it, and `crash_mc` never mounts a database. They are scaffolding for the full study harness, not results.

Treat only `tests/` as evidence. The published numbers in [`docs/slate_qemu_benchmarks.md`](https://github.com/ja7ad/slate/blob/main/docs/slate_qemu_benchmarks.md) come from the QEMU harness, not from these binaries.

## Adding a test

The pattern that works: build a `SimFlash` + `SimCounter`, mount an engine over static-ish buffers (see `create_slate` in `gc_compaction.rs`), drive a workload, inject a fault, drop the engine, remount over the *same* `flash.mem`, and assert on what survived. The remount is the important part — asserting on in-RAM state proves nothing about durability.

`proptest` is already a dependency; prefer a property over a fixed scenario when the invariant is universal ("for any crash point…").

## Report references

Crash model and prefix-durability §4 and §4.4 · GC and Invariant (T) §3.7 · write amplification §8.3.1 · erasure recovery §7. See [`docs/SLATE_FORMAL_SPECIFICATION.md`](https://github.com/ja7ad/slate/blob/main/docs/SLATE_FORMAL_SPECIFICATION.md) and [`docs/SIMULATION.md`](https://github.com/ja7ad/slate/blob/main/docs/SIMULATION.md).

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
