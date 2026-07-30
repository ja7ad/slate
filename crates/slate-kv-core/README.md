# slate-kv-core

[![crates.io](https://img.shields.io/crates/v/slate-kv-core.svg)](https://crates.io/crates/slate-kv-core)
[![docs.rs](https://docs.rs/slate-kv-core/badge.svg)](https://docs.rs/slate-kv-core)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**The [SLATE](https://github.com/ja7ad/slate) engine.** Append-only log, partial-key cuckoo index, epoch hash chain, checkpoints, recovery, garbage collection and the commit scheduler — composed into one `#![no_std]`, `#![forbid(unsafe_code)]`, **zero-allocation** engine.

Every buffer is caller-provided. The crate never calls `alloc`, never spawns a thread, never touches hardware, and never names a file or a flash chip. It reaches storage only through the [`slate-kv-hal`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv-hal) traits, and cryptography only through the `Sealer` trait it defines here.

> **Want a database you can just open and use?** That is [`slate-kv`](https://crates.io/crates/slate-kv). This crate is the engine underneath it — the level you work at when writing firmware or a custom backend.

The normative on-flash format, operational semantics and constant values are in [`../../docs/specification.md`](../../docs/specification.md).

## Install

```sh
cargo add slate-kv-core
```

```toml
[dependencies]
slate-kv-core = "0.5"
```

### Features

| Feature    | Default | Effect                                                                                                                                                                                                                                                                               |
|------------|---------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `blocking` | **on**  | Compiles the synchronous façade: `mount`, `seal_epoch`, `Log::commit`, and the `Slate::*` methods without the `_async` suffix, each a `task::block_on` projection of the async original. With it off, `mount` and `seal_epoch` are re-exports of `mount_async` / `seal_epoch_async`. |
| `metrics`  | off     | Enables the `Metrics` counters (commits, wakes, user/GC/parity/marker/checkpoint bytes, erases, GC tallies). With the feature off, `Metrics` is a zero-sized struct whose methods compile away, so a bare-metal build pays nothing.                                                  |
| `async`    | off     | Enables `slate-kv-hal/async`. **Currently inert in this crate**: no item here is gated on `feature = "async"` — the async API is unconditional (see below).                                                                                                                          |

### The API is async-first, and the blocking API is a projection of it

Each algorithm is written once as an `async fn` over `AsyncFlash` / `AsyncMonotonicCounter`. The `blocking` feature adds a same-named wrapper that drives that future with `task::block_on`, a busy-poll loop over a no-op waker. So `Slate::commit` and `Slate::commit_async` are the same code; only the driving differs.

Two consequences worth knowing before you build on it:

- Because `task::block_on` **busy-polls**, projecting a future over a flash driver that genuinely suspends will spin the CPU rather than sleep. The projection is only free when the underlying driver never returns `Pending` for an I/O reason — which is exactly the case for `slate_kv_hal::BlockingFlash`.
- Nothing in the type system enforces that pairing. `Slate` is a struct with public fields, so a caller can put a genuinely-async flash behind the blocking methods. Keep `BlockingFlash`/`BlockingCounter` with the blocking methods and native `AsyncFlash` with the `_async` methods.

## What "heapless" means for you as a caller

You own the memory; the engine borrows it for the lifetime of the `Slate`:

| Buffer              | Type            | Sizing                                                                                                                                                                                                                                                                             |
|---------------------|-----------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Hot log batch       | `&'a mut [u8]`  | Your choice. One record needs at most `REC_OVERHEAD + MAX_KEY_LEN + MAX_VAL_LEN` = 1,324 B; `BatchBuf::alloc` returns `Error::BatchFull` when the batch is full, and the scheduler commits before that in normal operation. `slate-kv` uses 64 KiB; the ESP32 firmware uses 4 KiB. |
| Cold (GC) log batch | `&'a mut [u8]`  | Same, sized independently.                                                                                                                                                                                                                                                         |
| Index slots         | `&'a mut [u32]` | Exactly `n_buckets * BUCKET_SLOTS`, with `n_buckets` a power of two.                                                                                                                                                                                                               |
| Checkpoint scratch  | `&'a mut [u8]`  | `config::ckpt_len_for_slots(index_slots.len())` — header, whole serialized index, and AEAD tag.                                                                                                                                                                                    |

`ScratchWorkspace` is different: it is an inline field of `Slate`, not a borrow, and it is where the record-staging and AEAD scratch buffers live so they are not stack-allocated across an `await`. It costs 5,720 B on a 32-bit target, so it is a real term in your budget, not free.

### RAM: the shipped configuration does not meet the 64 KiB target

With `n_buckets = 2048` (the `N_BUCKETS` default, and the floor `slate-kv`'s `Db::open` can produce), the engine needs roughly **81 KiB resident and 86 KiB at the mount peak** against a documented 64 KiB budget — over by 26.8% and 33.9%. The dominant term is not the index arena (32,768 B) but the **checkpoint buffer at 32,900 B**, which must hold the entire serialized index: any accounting that counts the arena and forgets the buffer understates the engine by about a factor of two.

The largest table that fits 64 KiB is `n_buckets = 1024` (≈49 KiB resident, ≈3,891 keys at α = 0.95), and that size is reachable only on a bare-metal target that sizes the arena directly — not through `Db::open`. See [`../../docs/specification.md`](../../docs/specification.md) § 4.4 for the per-term table and § 7.5 for the gap.

## Module map

| Module       | What it owns                                                                                                                                                                                        |
|--------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `record`     | On-flash record layout and the 28-byte `RecordHeader` codec (`magic ‖ seq ‖ op ‖ fp ‖ klen ‖ vlen ‖ nonce`), which is the AEAD associated data. `record_nonce(seq, epoch)` builds the 96-bit nonce. |
| `segment`    | `SegmentHeader` (59 B) codec, `Segment` block addressing, `encode_parity`.                                                                                                                          |
| `log`        | `Log::append` / `Log::commit`, `BatchBuf`, the doubled commit markers, the XOR head-parity page, `CommitBytes` accounting, and the **`Sealer` trait** the crypto layer implements.                  |
| `index`      | Partial-key cuckoo table: 4 slots/bucket, 8-bit fingerprint + 24-bit offset packed in one `u32`, xor-displacement, 8-entry stash. Also `h64` (FNV-1a) and `fingerprint`.                            |
| `chain`      | Epoch hash chain: `Chain::anchor(e, d_ckpt)` re-anchors per epoch, `Chain::fold(record)` is O(1) per record.                                                                                        |
| `epoch`      | `mount`, `seal_epoch`, `MountInfo`, `EngineState`, `SecurityMode`, `MountError`, `CkptCost`; the checkpoint-then-increment seal ordering and the boot freshness rule.                               |
| `checkpoint` | `CheckpointHeader` encode/decode over the sealed index snapshot.                                                                                                                                    |
| `recover`    | `recover` — replay from the newest checkpoint to the first erased page, torn-tail truncation — plus `RecoverWorkspace`, `PendingBatch`, `record_key_eq`, `scan_segment_headers`.                    |
| `gc`         | `SegTable`, `SegState`, victim selection (`pick_victim`, `pick_victim_excluding`), `compact_one_async`, `segments_in`.                                                                              |
| `sched`      | `Scheduler`, `RateEst`, and `b_star(λ, A, c)` — the energy-optimal batch size, computed in integer arithmetic with a deadline clamp.                                                                |
| `slate`      | `Slate<'a, F, C, S>`, the composed handle, and `ScratchWorkspace`.                                                                                                                                  |
| `task`       | `yield_now()` and `block_on()` — the cooperative yield point and the busy-poll bridge the blocking façade uses.                                                                                     |
| `repair`     | `head_repair_one_page` for a located erasure on the open head page. `scrub` is a stub.                                                                                                              |
| `config`     | Every tunable and format constant as a named `const`, the `ckpt_*` / `data_base_offset` layout helpers, and `SlateConfig::validate`.                                                                |
| `error`      | `Error` — `Tampered`, `Rollback`, `TornTail`, `BatchFull`, `FlashFull`, `WearOut`, `CounterExhausted`, `Io`, `FormatError`, `IndexFull`.                                                            |
| `metrics`    | Feature-gated counters feeding the write-amplification and energy accounting.                                                                                                                       |

## Assembling the engine

The engine is a **toolkit a host composes**, not a turnkey object. You assemble `Slate` once at boot, in this order. `F`, `C` and `S` below are your `AsyncFlash`, `AsyncMonotonicCounter` and `Sealer` — with the `blocking` feature, `BlockingFlash<YourFlash>` and `BlockingCounter<YourCounter>`.

```rust,ignore
use slate_kv_core::config::{self, SchedCfg};
use slate_kv_core::epoch::{self, EngineState, MountError};
use slate_kv_core::gc::{self, SegTable};
use slate_kv_core::index::{Index, XorShift64};
use slate_kv_core::log::{HeadState, Log, Sealer};
use slate_kv_core::metrics::Metrics;
use slate_kv_core::recover::{self, RecoverWorkspace};
use slate_kv_core::sched::Scheduler;
use slate_kv_core::slate::{ScratchWorkspace, Slate};

// The log begins above the superblock and the CKPT_SLOTS checkpoint slots.
let data_base = config::data_base_offset(block_size);

// 1. Mount: an O(1) freshness check on the counter and the newest checkpoint,
//    then load that checkpoint. `MountInfo` carries the rebuilt EngineState,
//    the plaintext length of the index snapshot, and where the tail begins.
//    Err(MountError::FormatError) means "blank medium" — format it by sealing a
//    genesis epoch. Tampered / Rollback are attack signals: stop, do not retry.
let (mut state, plain_len, replay_from, ckpt_seg_seq) =
    match epoch::mount(&mut flash, &mut counter, &mut sealer, &mut ckpt_buf) {
        Ok(mi) => (
            mi.state,
            mi.plain_len,
            mi.ckpt_write_offset.max(data_base),
            mi.ckpt_seg_seq,
        ),
        Err(MountError::FormatError) => { /* seal a genesis epoch; see below */ }
        Err(e) => return Err(e),
    };
sealer.roll_epoch(state.epoch);

// 2. Assemble the handle over your buffers. Every field is public and must be
//    supplied; there is no builder and no Default.
let head = || HeadState { seg_seq: 1, write_offset: data_base, block_idx: 0 };
let mut slate = Slate {
    flash, counter, sealer,
    engine: state,
    log_hot: Log::new(hot_buf, head()),
    log_cold: Log::new(cold_buf, head()),
    index: Index::new(index_slots, n_buckets),
    segs: SegTable::with_base(data_base, gc::segments_in(data_base, capacity)),
    ckpt_seg_seq,
    sched: Scheduler::new(sched_cfg),
    metrics: Metrics::default(),
    ckpt_buf,
    rng: XorShift64::new(seed),
    scratch_buf: ScratchWorkspace::new(),
};

// 3. Restore the index from the snapshot, then replay the committed tail.
if plain_len > 0 {
    let hdr = config::CKPT_HDR_LEN;
    slate.index.deserialize(&slate.ckpt_buf[hdr..hdr + plain_len]);
}
let mut rng = XorShift64::new(seed);
let mut workspace = RecoverWorkspace::new();
let info = recover::recover(
    &mut slate.flash, &mut slate.sealer, &mut slate.engine.chain,
    slate.engine.epoch, replay_from, &mut workspace,
    |flash, sealer, _seq, off, op, key| {
        // Deduplicate by the FULL key: fingerprints collide, so a candidate
        // record must be AEAD-opened and compared before its slot is reused.
        if op == config::OP_PUT {
            let _ = slate.index.upsert(key, off, &mut rng, |cand_off| {
                recover::record_key_eq(flash, sealer, cand_off, key)
            });
        } else {
            slate.index.remove(key, |cand_off| {
                recover::record_key_eq(flash, sealer, cand_off, key)
            });
        }
    },
)?;
// NEVER let next_seq regress — see rule 3 below.
slate.engine.next_seq = (info.committed_upto + 1).max(slate.engine.next_seq);
```

Two complete, working instances of this sequence exist: [`slate-kv`'s `Db::open`](https://github.com/ja7ad/slate/blob/main/crates/slate-kv/src/db.rs) (heap buffers, file-backed flash) and [`targets/esp32`](https://github.com/ja7ad/slate/tree/main/targets/esp32) (`static mut` buffers). Read `Db::open` as the reference host — including its genesis-format branch, which the block above elides.

Steady-state operation then goes through the log, index and scheduler:

```rust,ignore
// Append into the batch, fold the record into the chain, update the RAM index.
let offset = slate.append_cold(key, value, now_ms)?;
slate.index_update_offset(key, offset)?;

// Let the scheduler decide when the batch is worth a flash wake-up.
if slate.sched.on_append(now_ms) {
    slate.commit()?;      // ← the acknowledgement point
}

// Reads copy into a caller-owned buffer; no allocation on the read path.
let mut out = [0u8; slate_kv_core::config::MAX_VAL_LEN];
if let Some(n) = slate.get_into(key, &mut out) {
    let value = &out[..n];
}
```

`append_hot` is the low-level variant that takes an explicit `op` byte and does not consult the scheduler; `append_cold` / `append_cold_tombstone` are the GC-side log. Each has an `_async` twin.

## Four rules the engine will not enforce for you

These are invariants the correctness argument depends on. The type system cannot check them, so they are the host's responsibility:

1. **Acknowledge only after `commit` returns `Ok`.** `append` puts a record in a RAM batch; it is not durable. Reporting "written" before the commit marker is on flash breaks prefix durability — the guarantee everything else rests on. This is why `slate-kv` exposes both `put` (batched) and `put_durable` (put + commit).
2. **One writer.** `seq` is simultaneously the total order, the AEAD nonce source and the replay order, and there is no lock in this crate. Two concurrent writers reuse nonces — a confidentiality break, not a race you can retry.
3. **Never let `next_seq` regress.** After recovery take the maximum of the checkpoint's `seq` and the replayed tail. A crash immediately after a checkpoint leaves `committed_upto == 0` alongside a non-trivial checkpoint `seq`; reusing those numbers reuses nonces.
4. **Surface `Tampered` and `Rollback` unchanged.** Do not fold them into "I/O error" and do not retry them. They mean the flash image was altered or replaced.

## Index behaviour, measured

The index is the part of the engine whose cost is most often assumed rather than measured, so the numbers matter:

- **Lookup is exactly 16 slot probes** — `2 * BUCKET_SLOTS + STASH_SIZE` — for every key, at every load factor. `candidates()` scans both buckets and the whole stash unconditionally, so mean equals worst case. This is a constant, not a bound.
- **4.21 arena bytes per key** at the α = 0.95 design point, across every table size.
- **Mean load factor 0.986 at the first insertion failure** (the stash absorbs 8 entries beyond the arena, so a measured factor can slightly exceed 1.0).
- **Fingerprint collisions depend on your key names.** `fingerprint()` is the top byte of FNV-1a (`FP_BITS = 8`). For well-mixed keys the false-candidate rate is 0.0293, just under the `2b · 2⁻ᶠ` = 0.03125 bound. For **sequential keys** such as `sensor_000123` it reaches **0.1768** at `n_buckets = 16384` — 5.7× the bound — because the high ordinal bytes stay constant and FNV-1a mixes them only through its multiply chain. The shipped `n_buckets = 2048` is in the benign regime; larger tables are not. A false candidate costs a wasted record read, not a wrong answer: `index_update_offset` and `get_into` compare the full key after AEAD-opening the candidate.

Numbers and reproduction commands: [`../../docs/specification.md`](../../docs/specification.md) § 6.11.

## Tunables

All in `config`. The engine reads these as compile-time constants; `slate-kv`'s `Options` is a separate runtime layer on top.

| Constant                                | Value        | Meaning                                                                                                                                                                        |
|-----------------------------------------|--------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `B_COMMIT`                              | 27           | Default commit batch size (the ESP32 operating point). `slate-kv`'s `Options::default()` uses 8, and `sched::b_star` computes `B* = √(2λA/c)` at runtime when `auto_b` is set. |
| `B_MAX`                                 | 128          | Upper clamp on the scheduler's batch size.                                                                                                                                     |
| `THETA`                                 | 16,384       | Records per epoch: bounds the replay tail, the rollback window, and counter consumption.                                                                                       |
| `FP_BITS` / `OFF_BITS`                  | 8 / 24       | Fingerprint and offset bits in the packed `u32` slot. `OFF_BITS = 24` also caps addressable capacity at 16 MiB — `mount` rejects a larger volume with `FormatError`.           |
| `BUCKET_SLOTS` / `STASH_SIZE`           | 4 / 8        | Lookup cost is exactly `2b + s` = 16 slots.                                                                                                                                    |
| `N_BUCKETS`                             | 2,048        | Default table size (32,768 B arena, ≈7,782 keys at α = 0.95).                                                                                                                  |
| `MAX_KICKS`                             | 500          | Cuckoo relocations attempted before the stash absorbs the entry.                                                                                                               |
| `SEG_BLOCKS_DATA` / `SEG_BLOCKS_PARITY` | 8 / 4        | RS(12,8) stripe geometry; `SEG_BYTES` = 49,152.                                                                                                                                |
| `MAX_KEY_LEN` / `MAX_VAL_LEN`           | 256 / 1,024  | Hard record bounds. `RecordHeader::decode` rejects anything larger.                                                                                                            |
| `REC_HDR_LEN` / `TAG_LEN` / `CM_LEN`    | 28 / 16 / 83 | Record header, AEAD tag, commit marker.                                                                                                                                        |
| `CKPT_SLOTS` / `CKPT_HDR_LEN`           | 2 / 76       | Checkpoint slots are written alternately, so a torn checkpoint never destroys the previous one.                                                                                |
| `MAX_INDEX_SLOTS`                       | 65,536       | The whole index must fit one checkpoint slot, which is what caps index capacity.                                                                                               |

`SlateConfig::validate()` catches the two configuration mistakes that otherwise surface months into a deployment: an arena under twice the expected live bytes (`CapacityTooSmall`, because GC needs headroom) and a counter budget too small for the expected lifetime in ops (`CounterBudgetExceeded`), plus an inconsistent scheduler config (`InvalidSchedCfg`).

## What this crate does not do

Kept here so nothing gets planned around a capability that is not there:

- **Reclaimed space is not reusable.** GC frees and erases segments correctly, but the log head cannot wrap into freed space, so a device eventually halts with `FlashFull` while most segments are free and erased. Two format-level causes: records straddle segment boundaries, and tail replay scans forward to the first erased page, so a wrapped head would be unreplayable. `recover::scan_segment_headers` — the ordering mechanism a circular log needs — exists, but **nothing ever writes segment headers**. Closing this requires a format change.
- **`repair::scrub` is a stub** returning `Ok(())`. `head_repair_one_page` is implemented; a background scrub pass is not.
- **`slate::Slate::index_points_to` is a simplified check** — it tests only whether a candidate list contains the offset, without opening the record to compare the full key.
- **Mount replay does not yield.** The recovery path is on the blocking flash trait, so replaying a full Θ = 16,384-record tail is one uninterruptible span.
- **No allocator, no executor, no timer.** You supply `now_ms` to the scheduler; the engine never reads a clock itself.

## Testing

```sh
cargo test -p slate-kv-core                                    # unit tests
cargo build -p slate-kv-core --target thumbv7em-none-eabihf    # no_std purity (make build-bare)
cargo test -p slate-kv-sim                                     # crash injection, GC, RS recovery
cargo run --release -p slate-kv-core --example slate_index     # the index measurements above
cargo +nightly fuzz run fuzz_record_decode                     # decoders, from fuzz/
```

CI additionally runs Miri and a bare-metal matrix build; this crate must build with no `std` and no `alloc` on every change.

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
