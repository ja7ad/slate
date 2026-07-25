# slate-kv-core

[![crates.io](https://img.shields.io/crates/v/slate-kv-core.svg)](https://crates.io/crates/slate-kv-core)
[![docs.rs](https://docs.rs/slate-kv-core/badge.svg)](https://docs.rs/slate-kv-core)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**The [SLATE](https://github.com/ja7ad/slate) engine.** Append-only log, cuckoo index, hash chain, epochs, checkpoints, recovery, GC and the commit scheduler — layers L1–L5 composed into one `#![no_std]`, `#![forbid(unsafe_code)]`, **zero-allocation** engine.

Every buffer is caller-provided. The crate never calls `alloc`, never spawns a thread, never touches hardware, and never names a file or a flash chip. It talks to storage only through the [`slate-kv-hal`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv-hal) traits and to crypto only through the `Sealer` trait it defines.

> **Looking for a database you can just open and use?** You want [`slate-kv`](https://crates.io/crates/slate-kv). This crate is the engine underneath it — the level you work at when writing firmware or a custom backend.

## Install

```sh
cargo add slate-kv-core
```

```toml
[dependencies]
slate-kv-core = "0.3"
```

### Features

| Feature | Default | Effect |
|---|---|---|
| `metrics` | off | Enables the `Metrics` counters (commits, wakes, user/GC/parity/checkpoint bytes, erases). With the feature off, `Metrics` is a zero-sized struct whose methods compile away — a bare-metal build pays nothing. |

## What "heapless" means for you as a caller

You own the memory. The engine borrows it:

| Buffer | Type | Sized by |
|---|---|---|
| Hot log batch | `&'a mut [u8]` | `B_MAX × (REC_OVERHEAD + MAX_KEY_LEN + MAX_VAL_LEN)` |
| Cold (GC) log batch | `&'a mut [u8]` | same |
| Index slots | `&'a mut [u32]` | `n_keys / α`, rounded to a power of two × `BUCKET_SLOTS` |
| Checkpoint scratch | `&'a mut [u8]` | index slot bytes + header |

On a microcontroller these are `static mut` arrays or a `SyncBuffer`; on an OS they are boxed slices. Either way the working set is fixed at build time and visible in the map file — which is the point. Target for an ESP32-class configuration is **≤ 64 KB total**, with the index at roughly **4.5 bytes per key**.

## Module map

| Module | What it owns | Report |
|---|---|---|
| `record` | On-flash record layout: `magic ‖ seq ‖ op ‖ h(k) ‖ klen ‖ vlen ‖ nonce ‖ AEAD(k‖v) ‖ τ`; the 28-byte header is the AEAD associated data | §3.1, §3.3 |
| `segment` | Segment lifecycle: open head, seal, RS parity computation, erased-state scanning | §3.1, §3.5 |
| `log` | `Log::append` / `Log::commit`, the batch buffer, doubled commit markers, XOR head-parity page, and the **`Sealer` trait** | §3.1, §3.5 |
| `index` | Partial-key cuckoo table — 4 slots/bucket, 8-bit fingerprint + 24-bit offset packed into a `u32`, xor-displacement, stash | §3.2, §5 |
| `chain` | Epoch hash chain `χᵢ = H(χᵢ₋₁ ‖ rᵢ)`, re-anchored per epoch | §3.4 |
| `epoch` | `mount`, `seal_epoch`, `SecurityMode`, `MountError`; checkpoint-then-increment ordering | §3.4 |
| `checkpoint` | Sealed index snapshot header, encode/decode | §3.6 |
| `recover` | Boot replay from the last checkpoint, torn-tail truncation, `record_key_eq` | §4 |
| `gc` | `SegTable`, victim selection, `compact_one`, tombstone Invariant (T) | §3.7, §8.3 |
| `sched` | `Scheduler`, `b_star(λ, A, c)` — the energy-optimal batch size, deadline clamp | §8 |
| `repair` | Located-erasure repair of the open head page (`scrub` is a stub — see below) | §7 |
| `config` | Every tunable as a named constant with its report reference, plus `SlateConfig::validate` | §9 |
| `error` | `Error` — `Tampered`, `Rollback`, `TornTail`, `FlashFull`, `WearOut`, `CounterExhausted`, … | — |
| `metrics` | Feature-gated counters feeding the energy model | §8 |

## Using the engine

The engine is a **toolkit that a host composes**, not a turnkey object. `Slate` is the struct that holds the composed state; you assemble it once at boot, in this order:

```rust
use slate_kv_core::{config, epoch, index::Index, log::{HeadState, Log}, recover, slate::Slate};

// 1. Mount: O(1) freshness check on the chain tip + counter, then load the checkpoint.
//    Err(MountError::FormatError) means "blank medium" — call seal_epoch to format.
//    Err(MountError::Tampered) / Err(MountError::Rollback) are attack signals: stop.
let (engine_state, plain_len) = epoch::mount(&mut flash, &mut counter, &mut sealer, ckpt_buf)?;

// 2. Assemble the handle over your buffers.
let mut slate = Slate {
    flash, counter, sealer,
    engine: engine_state,
    log_hot: Log::new(hot_buf, HeadState { seg_seq: 1, write_offset: 0, block_idx: 0 }),
    log_cold: Log::new(cold_buf, HeadState { seg_seq: 1, write_offset: 0, block_idx: 0 }),
    index: Index::new(index_slots, n_buckets),
    /* segs, sched, metrics, ckpt_buf, rng, ckpt_seg_seq */
};

// 3. Restore the index from the checkpoint snapshot, then replay the committed tail.
slate.index.deserialize(&slate.ckpt_buf[header_len..header_len + plain_len]);
let info = recover::recover(
    &mut slate.flash, &mut slate.sealer, &mut slate.engine.chain,
    slate.engine.epoch, config::data_base_offset(block_size), &mut workspace,
    |flash, sealer, _seq, off, op, key| { /* upsert on OP_PUT, remove on OP_DEL */ },
)?;
slate.engine.next_seq = (info.committed_upto + 1).max(slate.engine.next_seq);
```

[`slate-kv`'s `Db::open`](https://github.com/ja7ad/slate/blob/main/crates/slate-kv/src/db.rs) is a complete, working instance of this sequence — read it as the reference host. `targets/esp32` is the same sequence against static buffers.

Steady-state operations then go through the log and index directly:

```rust
// Write: append to the batch, fold into the chain, update the RAM index.
let (seq, offset) = slate.log_hot.append(
    seq, config::OP_PUT, key, value, &mut slate.sealer, &mut slate.engine.chain,
)?;
slate.engine.next_seq += 1;
slate.index_update_offset(key, offset)?;

// Let the scheduler decide when the batch is worth the flash wake-up.
if slate.sched.on_append(now_ms) {
    slate.commit()?;   // ← the acknowledgement point
}
```

## Four rules the engine will not enforce for you

These are invariants the proofs depend on. The type system cannot check them, so they are your responsibility as a host:

1. **Acknowledge only after `commit` returns `Ok`.** `append` puts a record in a RAM batch; it is not durable. Telling a caller "written" before the commit marker is on flash breaks prefix-durability (Theorem 4.1) — the one guarantee everything else builds on. This is why [`slate-kv`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv) exposes both `put` (batched) and `put_durable` (put + commit).
2. **One writer.** `seq` is the total order, the AEAD nonce source, and the replay order all at once. There is no lock inside this crate. Two concurrent writers reuse nonces, which is a confidentiality break, not a race you can retry.
3. **Never let `next_seq` regress.** After recovery, take the max of the checkpoint's `seq` and the replayed tail. A crash right after a checkpoint leaves `committed_upto == 0` with a non-trivial checkpoint seq; reusing those numbers reuses nonces.
4. **Surface `Tampered` and `Rollback` as-is.** Do not collapse them into "I/O error" and do not retry them. They mean the flash image was altered or replaced.

## Tunables

All in `config`, each traceable to a section of the formal report. Defaults are the Pareto picks from §9:

| Constant | Default | Meaning |
|---|---|---|
| `B_COMMIT` | 27 | Commit batch size (ESP32 pick; Pi uses 9). `sched::b_star` computes `B* = √(2λA/c)` at runtime when `auto_b` is set |
| `THETA` | 16 384 | Ops per epoch: bounds boot replay, the rollback window, and counter consumption |
| `FP_BITS` / `OFF_BITS` | 8 / 24 | Index fingerprint and offset bits — RAM vs. wasted-read rate `2b·2⁻ᶠ` |
| `BUCKET_SLOTS` / `STASH_SIZE` | 4 / 8 | Worst-case lookup ≤ `2b + s` slots |
| `SEG_BLOCKS_DATA` / `SEG_BLOCKS_PARITY` | 8 / 4 | RS(12,8) stripe geometry |
| `MAX_KEY_LEN` / `MAX_VAL_LEN` | 256 / 1024 | Hard record bounds — the batch buffer is sized from these |

`SlateConfig::validate()` catches the configuration mistakes that would otherwise show up months into a deployment: a flash arena too small for the target utilisation (`CapacityTooSmall`), and a monotonic-counter budget too small for the expected lifetime in ops (`CounterBudgetExceeded`).

## Status notes

Kept here so nobody plans around something that is not there yet:

- `repair::scrub` is a stub returning `Ok(())`. Head-page erasure repair (`head_repair_one_page`) is implemented; a full background scrub pass is not.
- `slate::Slate::index_points_to` is a simplified check; see its inline note.

## Testing

```sh
cargo test -p slate-kv-core                                    # unit tests
cargo build -p slate-kv-core --target thumbv7em-none-eabihf    # no_std purity (also `make build-bare`)
cargo test -p slate-kv-sim                                     # crash injection, GC, RS recovery
cargo +nightly fuzz run fuzz_record_decode                     # decoders, from fuzz/
```

CI additionally runs Miri and a bare-metal matrix build; `slate-kv-core` must build with no `std` and no `alloc` on every change.

## Report references

The full formal model, theorems, and cost models are in [`docs/SLATE_FORMAL_SPECIFICATION.md`](https://github.com/ja7ad/slate/blob/main/docs/SLATE_FORMAL_SPECIFICATION.md); crate boundaries and data flows in [`docs/ARCHITECTURE.md`](https://github.com/ja7ad/slate/blob/main/docs/ARCHITECTURE.md).

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
