# slate-kv

[![crates.io](https://img.shields.io/crates/v/slate-kv.svg)](https://crates.io/crates/slate-kv)
[![docs.rs](https://docs.rs/slate-kv/badge.svg)](https://docs.rs/slate-kv)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**[SLATE](https://github.com/ja7ad/slate) for any operating system.** An encrypted, tamper-evident, crash-safe key-value store in a single embeddable crate — Linux, Raspberry Pi, macOS, Windows. This is the crate you want if you are writing an application rather than firmware.

Same engine as the bare-metal build ([`slate-kv-core`](https://crates.io/crates/slate-kv-core)), wrapped with a file-backed flash emulation, heap conveniences, and a `Send + Sync` handle.

## Install

```sh
cargo add slate-kv
```

```toml
[dependencies]
slate-kv = "0.3"
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
        Options::default(),
    )?;

    db.put(b"sensor_1", b"23.5 C")?;    // buffered — NOT yet durable
    db.commit()?;                       // ← the acknowledgement point

    if let Some(v) = db.get(b"sensor_1")? {
        println!("sensor_1 = {}", String::from_utf8_lossy(&v));
    }

    db.delete(b"sensor_1")?;
    db.commit()?;
    Ok(())
}
```

## The one thing to get right: `put` vs `put_durable`

```rust
db.put(b"k", b"v")?;          // appends to the in-RAM batch; may commit if the
                              // energy scheduler decides the batch is worth a wake-up
db.commit()?;                 // flushes the batch + commit marker; NOW it is durable

db.put_durable(b"k", b"v")?;  // exactly the two lines above, in one call
```

`put` returning `Ok` means *accepted*, not *durable*. A power cut before the next `commit` loses it. This is not an implementation shortcut — it is the whole reason SLATE is energy-efficient: batching amortises the fixed cost of waking flash across `B` operations, and the scheduler picks `B` to minimise joules per op.

The rule to build on: **do not acknowledge anything to your own caller until `commit()` has returned `Ok`.** What `commit` guarantees in return is strong — after any power loss, at any byte offset, recovery restores exactly the prefix of operations that were committed. Never a partial record, never a reordering, never a gap.

Use `put_durable` / `delete_durable` when a single write must survive the next microsecond. Use `put` + periodic `commit` for throughput.

## `Options`

```rust
use slate_kv::{Options, Profile};
use slate_kv::file_flash::Durability;

let opts = Options {
    capacity: 4 * 1024 * 1024,   // bytes of the emulated flash arena (must exceed 2× live data)
    b_commit: 8,                 // fixed commit batch size; ignored when auto_b is true
    auto_b: true,                // let the scheduler compute B* = √(2λA/c) from the observed rate
    staleness_budget_ms: 1000,   // how long a buffered write may sit uncommitted
    n_keys: 8192,                // index capacity — sizes the cuckoo table
    profile: Profile::Pi,        // Pi or Esp32 energy constants (report §9)
    durability: Durability::Full,
};
```

`Durability::Full` uses `F_FULLFSYNC` on macOS and `fsync` elsewhere, so a returned `commit` really is on stable media. `Durability::OsCache` skips the drive-cache flush on macOS — faster, and only appropriate when you have battery-backed storage or are running tests.

Capacity is not a soft limit: writes fail with `FlashFull` when the arena is exhausted, and GC needs headroom, so keep `capacity ≥ 2 × expected live bytes`.

## Keys

```rust
KeySource::Bytes([u8; 32])       // caller-supplied, e.g. from an OS keystore
KeySource::File(PathBuf)         // first 32 bytes of a file
KeySource::Env("SLATE_KEY")      // first 32 bytes of an env var
```

The root key never touches the database directory. Every subkey — per-epoch record key, commit-marker MAC key, checkpoint key, counter MAC key — is HKDF-derived from it with domain separation, and the derived set is zeroized on drop. Lose the key and the data is unrecoverable; that is the intended property.

Keys and values are sealed together *inside* the ciphertext (`k ‖ v`). Someone with the database file learns how many records exist and roughly how big they are — not what the keys are called.

## Reading the security mode

```rust
use slate_kv_core::epoch::SecurityMode;

match db.security_mode() {
    SecurityMode::Full                 => {} // hardware monotonic counter: rollback-protected
    SecurityMode::BestEffortRollback   => {} // file-backed counter — see below
    SecurityMode::NoRollbackProtection => {} // no counter available
}
```

On an ordinary OS this reports **`BestEffortRollback`**, and you should design around that. The counter lives in `counter.bin`; an attacker who can restore an old *pair* of `data.bin` + `counter.bin` can roll the database back to an earlier epoch, and SLATE cannot tell. Tamper-*evidence* (any edit to the log is detected) and confidentiality are unaffected — only rollback protection degrades, and the engine says so rather than quietly claiming a guarantee it does not have.

`SecurityMode::Full` needs a hardware anchor: an eFuse counter (see [`targets/esp32`](https://github.com/ja7ad/slate/tree/main/targets/esp32)), TPM, or RPMB.

## Errors

```rust
pub enum DbError {
    Core(slate_kv_core::error::Error),   // Tampered, Rollback, FlashFull, TornTail, …
    Mount(slate_kv_core::epoch::MountError),
    Io(std::io::Error),
    Config(String),
    InvalidArg(String),
}
```

`Tampered` and `Rollback` are **security signals, not transient failures**. They mean the database file was altered or replaced since it was last written. Do not retry, do not fall back to a fresh database, do not log them at the same level as an I/O error — surface them to whoever owns the device.

`InvalidArg` covers a key over 256 bytes or a value over 1024 bytes (`MAX_KEY_LEN` / `MAX_VAL_LEN` in `slate-kv-core::config`).

## Concurrency

`Db` is `Send + Sync` and every method takes `&self`, so you can share it across threads with an `Arc`. Internally a mutex serialises access, because the engine is single-writer by design: the sequence number is simultaneously the total order, the AEAD nonce, and the replay order.

Concurrent readers therefore contend with writers. If read throughput matters more than write latency, batch your writes.

## Operations

```rust
db.compact()?;               // one GC pass: copy live records forward, erase a victim segment
db.len();                    // live key count
db.epoch();                  // current epoch
db.acked_seq();              // last durable sequence number
db.stats();                  // commits, wakes, user/GC/parity/checkpoint bytes, erases
```

`stats()` is what the energy model consumes: `wakes` and `bytes` are the terms in the joules-per-op formula, and `gc_bytes / user_bytes` is your measured write amplification.

`db.scrub()` currently returns an empty `ScrubReport` — background scrubbing is designed but not yet implemented.

## Crash safety in practice

Recovery runs automatically on `Db::open`: an O(1) freshness check on the chain tip and counter, then a bounded O(Θ) replay from the last checkpoint, truncating any torn tail. There is no repair tool to run and no "unclean shutdown" mode — reopening *is* the recovery.

The [`kill9` test](https://github.com/ja7ad/slate/blob/main/crates/slate-kv/tests/kill9.rs) does this the honest way: it `SIGKILL`s a writing process and asserts the reopened database contains exactly the acknowledged prefix.

## When to use something else

If you are on a server, want high write throughput, and do not need at-rest tamper-evidence or a bounded RAM footprint, RocksDB or SQLite will serve you better. SLATE trades raw throughput for proven crash-safety, encryption-by-default, tamper-evidence, and a working set small enough to fit on a microcontroller.

## Related crates

| Crate | Use it for |
|---|---|
| [`slate-kv-core`](https://crates.io/crates/slate-kv-core) | Bare-metal / `no_std` hosts |
| [`slate-kv-cli`](https://crates.io/crates/slate-kv-cli) | Inspecting a database from a shell |
| [`slate-kv-ffi`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv-ffi) | C ABI, and the base for the Go/other bindings |

## Testing

```sh
cargo test -p slate-kv                                    # round-trip, security, kill9
cargo run -p slate-kv --example pi_bench --release        # throughput / energy bench
```

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
