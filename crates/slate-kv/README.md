# slate-kv

[![crates.io](https://img.shields.io/crates/v/slate-kv.svg)](https://crates.io/crates/slate-kv)
[![docs.rs](https://docs.rs/slate-kv/badge.svg)](https://docs.rs/slate-kv)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**[SLATE](https://github.com/ja7ad/slate) for any operating system.** An encrypted, tamper-evident, crash-safe key-value store in a single embeddable crate — Linux, Raspberry Pi, macOS, Windows. This is the crate you want if you are writing an application rather than firmware.

Same engine as the bare-metal build ([`slate-kv-core`](https://crates.io/crates/slate-kv-core)), wrapped with a file-backed flash emulation, heap-allocated buffers, and a `Send + Sync` handle.

The normative format and semantics are in [`../../docs/specification.md`](../../docs/specification.md).

## Install

```sh
cargo add slate-kv
```

```toml
[dependencies]
slate-kv = "0.5"
```

## Quickstart

```rust
use slate_kv::{Db, KeySource, Options};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `path` is a DIRECTORY. SLATE creates `data.bin` (the log) and
    // `counter.bin` (the rollback anchor) inside it.
    let db = Db::open(
        Path::new("./slate_db"),
        KeySource::Bytes([0x42; 32]),   // 32-byte root key — see "Keys" below
        Options::default(),             // 4 MiB, b_commit 8, auto_b, Pi profile
    )?;

    db.put(b"sensor_1", b"23.5 C")?;    // buffered — NOT yet durable
    db.commit()?;                       // ← the acknowledgement point

    if let Some(v) = db.get(b"sensor_1")? {
        println!("sensor_1 = {}", String::from_utf8_lossy(&v));
    }

    db.delete(b"sensor_1")?;
    db.commit()?;

    // Rollback protection degrades honestly; ask what you actually have.
    println!("security mode: {:?}", db.security_mode());
    Ok(())
}
```

`DbError` implements `Display` and `std::error::Error`, so `?` into `Box<dyn Error>` works and error messages print without `{:?}`.

## The one thing to get right: `put` vs `put_durable`

```rust,ignore
db.put(b"k", b"v")?;          // appends to the in-RAM batch; commits only if
                              // the energy scheduler decides the batch is worth
                              // a flash wake-up, or the staleness deadline hits
db.commit()?;                 // flushes the batch + commit marker; NOW durable

db.put_durable(b"k", b"v")?;  // exactly the two calls above, in one
```

`put` returning `Ok` means *accepted*, not *durable*. A power cut before the next `commit` loses it. This is not an implementation shortcut — it is the reason SLATE is energy-efficient: batching amortises the fixed cost of waking flash across `B` operations, and the scheduler picks `B` to minimise joules per op.

The rule to build on: **do not acknowledge anything to your own caller until `commit()` has returned `Ok`.** What `commit` gives back is strong — after any power loss, at any byte offset, recovery restores exactly a prefix of the committed operations, containing every operation committed before the crash. Never a partial record, never a reordering, never a gap. Measured across 20,000 power-loss trials with the cut at a uniformly random byte: zero violations ([specification § 6.3](../../docs/specification.md)).

Both `put_durable` and `delete_durable` commit unconditionally, so the write is acknowledged — `acked_seq` has advanced — by the time they return. The plain `put` and `delete` leave the record in the open batch and commit only when the scheduler fires; use them when you intend to amortise the commit across a batch, and see the write-amplification table below for why that matters.

Use `put_durable` when a single write must survive the next microsecond; use `put` plus a periodic `commit` for throughput.

### What batching costs on flash

Write amplification — bytes actually programmed per byte of user data — is dominated by commit markers at small batch sizes, not by garbage collection:

| `b_commit` | Write amplification | Marker share of bytes | GC share |
|-----------:|--------------------:|----------------------:|---------:|
|          1 |                6.31 |                 52.8% |    0.37% |
|          8 |                1.69 |                 24.2% |    0.24% |
|         16 |                1.38 |                 14.8% |    0.00% |
|        128 |                1.11 |                  2.3% |    0.00% |

The mechanism is page granularity: a marker is 83 logical bytes but occupies a whole 256 B page, and two copies are written, so every commit costs 512 B regardless of batch size. Garbage-collection relocation never exceeds 0.95% of bytes written anywhere in the sweep — the inverse of what the LSM and SSD literature would suggest. The practical consequence: **durability granularity, not compaction policy, is the primary lifetime lever on this class of device.** ([specification § 6.7](../../docs/specification.md).)

## `Options`

```rust,ignore
use slate_kv::file_flash::Durability;
use slate_kv::{Options, Profile};

let opts = Options {
    capacity: 4 * 1024 * 1024,   // bytes of the file-backed flash arena
    b_commit: 8,                 // fixed commit batch; ignored while auto_b
    auto_b: true,                // scheduler derives B* from the observed rate
    staleness_budget_ms: 1000,   // how long a buffered write may sit uncommitted
    n_keys: 8192,                // index capacity — sizes the cuckoo table
    profile: Profile::Pi,        // Pi or Esp32 energy constants and deadline
    durability: Durability::Full,
};
```

Those values are `Options::default()`. `Profile` selects the scheduler's fixed wake cost and deadline (Pi: 150 µJ / 500 ms; Esp32: 400 µJ / 1000 ms).

`Durability::Full` uses `F_FULLFSYNC` on macOS and `fsync` elsewhere, so a returned `commit` really is on stable media. `Durability::OsCache` skips the drive-cache flush on macOS — faster, and only appropriate with battery-backed storage or in tests. On Linux the two are identical by construction.

Capacity is not a soft limit: writes fail with `FlashFull` once the arena is exhausted, and GC needs headroom, so keep `capacity ≥ 2 × expected live bytes`.

`n_keys` is rounded up: the arena is `next_power_of_two(max(n_keys, 2048) / 0.95) * 4` slots, so 2,048 buckets is the floor even if you ask for fewer keys. Asking for more keys than one checkpoint slot can hold is rejected at `open` with a `Config` error naming the maximum, rather than degrading into a database that silently never checkpoints.

## Keys

```rust,ignore
KeySource::Bytes([u8; 32])       // caller-supplied, e.g. from an OS keystore
KeySource::File(PathBuf)         // first 32 bytes of a file
KeySource::Env("SLATE_KEY")      // first 32 bytes of an env var
```

The root key never touches the database directory. Every subkey — per-epoch record key, commit-marker MAC key, checkpoint key, counter MAC key — is HKDF-derived from it with domain separation, and the derived set is zeroized on drop. Lose the key and the data is unrecoverable; that is the intended property.

Keys and values are sealed together *inside* the ciphertext (`k ‖ v`). Someone holding the database file learns how many records exist and roughly how big they are — not what the keys are called.

## Reading the security mode

```rust,ignore
use slate_kv_core::epoch::SecurityMode;

match db.security_mode() {
    SecurityMode::Full                 => {} // hardware monotonic counter
    SecurityMode::BestEffortRollback   => {} // file-backed counter — see below
    SecurityMode::NoRollbackProtection => {} // no counter available
}
```

On an ordinary OS this reports **`BestEffortRollback`**, and you should design around that. The counter lives in `counter.bin`; an attacker who can restore an old *pair* of `data.bin` + `counter.bin` can roll the database back to an earlier epoch, and SLATE cannot tell. Tamper-*evidence* (any edit to the log is detected) and confidentiality are unaffected — only rollback protection degrades, and the engine says so rather than quietly claiming a guarantee it does not have.

Two further limits on rollback protection, true even with a hardware counter:

- **It is per epoch, not per record.** A rollback *within* the current epoch is not distinguished, so the window is bounded by `THETA` = 16,384 records.
- Detection of a spliced older image is what was tested: 5,000 splice attacks, all rejected ([specification § 6.4](../../docs/specification.md)).

`SecurityMode::Full` needs a hardware anchor: an eFuse counter (see [`targets/esp32`](https://github.com/ja7ad/slate/tree/main/targets/esp32)), TPM, or RPMB.

## Errors

```rust,ignore
pub enum DbError {
    Core(slate_kv_core::error::Error),        // Tampered, Rollback, FlashFull, …
    Mount(slate_kv_core::epoch::MountError),  // Rollback, Tampered, Io, FormatError
    Io(std::io::Error),
    Config(String),
    InvalidArg(String),
}
```

`Tampered` and `Rollback` — in either the `Core` or the `Mount` variant — are **security signals, not transient failures**. They mean the database was altered or replaced since it was last written. Do not retry, do not fall back to a fresh database, do not log them at the same level as an I/O error: surface them to whoever owns the device.

Because the store is authenticated, a **wrong key is indistinguishable from tampering**: both surface as a tag failure. A mount error on a database you believe is intact is worth double-checking against the key you passed.

`InvalidArg` covers a key over 256 bytes or a value over 1,024 bytes (`MAX_KEY_LEN` / `MAX_VAL_LEN` in `slate_kv_core::config`).

## Concurrency

`Db` is `Send + Sync` and every method takes `&self`, so you can share it across threads behind an `Arc`. Internally a mutex serialises access, because the engine is single-writer by design: the sequence number is simultaneously the total order, the AEAD nonce, and the replay order.

Concurrent readers therefore contend with writers. If read throughput matters more than write latency, batch your writes.

## Operations and observability

```rust,ignore
db.compact()?;               // one GC pass: relocate live records, erase a victim
db.seal_epoch()?;            // commit, then close the epoch and write a checkpoint
db.len();                    // live key count
db.is_empty();
db.epoch();                  // current epoch
db.acked_seq();              // last durable sequence number
db.next_seq();               // next sequence number to be issued
db.stats();                  // Stats: commits, wakes, per-bucket bytes, GC tallies
db.mount_report();           // MountReport: what the last open actually cost
```

`stats()` returns the write-amplification and energy inputs: `user_bytes`, `gc_bytes`, `parity_bytes`, `marker_bytes`, `ckpt_bytes`, `erases`, `commits`, `wakes`, plus segment-state and GC counters. `Stats::flash_bytes()` sums the byte buckets and `Stats::write_amplification()` returns `Option<f64>` — `None` before anything is written, because "not yet measured" and "no overhead" are different claims.

`mount_report()` separates mount's fixed cost from its tail cost: `flash_after_ckpt` is the flash counter snapshot taken just after the checkpoint load, so subtracting it from `flash` splits the O(1) checkpoint term from the O(tail) replay term. `records_replayed`, `scan_bytes`, `ckpt_slots_verified` and `key_verify_calls` are reported separately so a slow mount can be attributed rather than guessed at.

`db.scrub()` returns an all-zero `ScrubReport`: background scrubbing is designed but not implemented.

`seal_epoch()` is normally unnecessary — an epoch closes on its own every `THETA` records — but it lets you place a checkpoint at a known point in the log, which makes the replay-tail length a controllable variable when measuring mount cost.

## Crash safety in practice

Recovery runs automatically on `Db::open`: an O(1) freshness check on the chain tip and counter, then a bounded replay from the newest checkpoint, truncating any torn tail. There is no repair tool to run and no "unclean shutdown" mode — reopening *is* the recovery.

The [`kill9` test](https://github.com/ja7ad/slate/blob/main/crates/slate-kv/tests/kill9.rs) does this the honest way: it `SIGKILL`s a writing process and asserts the reopened database contains exactly the acknowledged prefix.

Dropping a `Db` performs a best-effort `commit` so a clean shutdown does not silently discard the open batch. It is a convenience, not a guarantee: `drop` cannot report an error, so anything you must know is durable needs an explicit `commit()` whose result you check.

## Known limitations

- **Reclaimed space is not reusable.** GC frees and erases segments, but the log head cannot wrap into freed space, so a long-running store eventually fails with `FlashFull` while most of its arena is free. Closing this requires an on-flash format change. Size `capacity` for total bytes written between reformats, not for peak live bytes.
- **RAM is larger than the embedded budget suggests.** The engine's resident working set is ~81 KiB at the default table size, dominated by a checkpoint buffer that holds the entire serialized index. This matters little on an OS host; it is the binding constraint on a microcontroller.
- **Sequential key names weaken the index fingerprint.** Keys like `sensor_000123` raise the false-candidate rate — a wasted record read, never a wrong answer — and the effect grows with table size. Well-mixed key names stay within the theoretical bound.
- **Host timings are host timings.** Any latency you measure here characterises your filesystem's barrier, not a device's flash.

## When to use something else

On a server, wanting high write throughput, and not needing at-rest tamper-evidence or a bounded RAM footprint? RocksDB or SQLite will serve you better. SLATE trades raw throughput for crash-safety it can demonstrate, encryption by default, tamper-evidence, and a working set small enough to fit on a microcontroller.

## Related crates

| Crate                                                                          | Use it for                                        |
|--------------------------------------------------------------------------------|---------------------------------------------------|
| [`slate-kv-core`](https://crates.io/crates/slate-kv-core)                      | Bare-metal / `no_std` hosts                       |
| [`slate-kv-cli`](https://crates.io/crates/slate-kv-cli)                        | Inspecting a database from a shell                |
| [`slate-kv-ffi`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv-ffi) | C ABI, and the base for the Go and other bindings |

## Testing

```sh
cargo test -p slate-kv                                    # round-trip, security, kill9
cargo run -p slate-kv --example pi_bench --release        # throughput / energy bench
cargo run -p slate-kv --example slate_wa_buckets --release # write-amplification buckets
```

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
