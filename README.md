# SLATE: Secure, Log-structured, Authenticated, Tamper-Evident Key–Value Engine

<p align="center">
  <img src="https://raw.githubusercontent.com/ja7ad/slate/main/docs/slate_logo.png" alt="SLATE Logo" width="300"/>
</p>

[![codecov](https://codecov.io/gh/ja7ad/slate/graph/badge.svg?token=leYIzSiuLf)](https://codecov.io/gh/ja7ad/slate)
[![CI](https://github.com/ja7ad/slate/actions/workflows/ci.yml/badge.svg)](https://github.com/ja7ad/slate/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)
[![no_std](https://img.shields.io/badge/rust-no__std-green.svg)](crates/slate-kv-core)

SLATE is a key-value storage engine for edge devices, from bare-metal microcontrollers like the ESP32 up to Linux-class boards. It targets a setting most storage engines are not designed for: raw NOR flash with no filesystem beneath it, no battery-backed cache, well under 100 KiB of RAM, and a power supply that can be interrupted in the middle of a page program.

SLATE composes well-understood primitives — log-structured storage, AEAD, cuckoo hashing, Reed–Solomon parity — into a design whose guarantees are *proven* rather than assumed, and it states where those guarantees stop. See [`docs/specification.md`](docs/specification.md) for the normative on-flash format, the operational semantics, and the conformance data behind every number quoted here.

## What SLATE guarantees

Three properties hold simultaneously, each with a proof and an empirical check:

- **Prefix durability.** After an arbitrary number of power failures at arbitrary instants, recovery returns exactly the state produced by some prefix of the acknowledged write sequence, containing every write acknowledged before the last crash. Verified across 20,000 power-loss trials with the cut at a uniformly random byte — zero violations.
- **Rollback resistance.** An at-rest adversary who replaces the flash with an older authentic image is detected: making recovery accept a stale epoch requires a MAC forgery. Protection is **per epoch** — a rollback *within* the current epoch is not distinguished, and the window is bounded by Θ. Verified across 5,000 splice attacks — all rejected.
- **Erasure tolerance.** Segment parity is maximum-distance-separable, so any `n-k` *declared* block erasures reconstruct exactly. Verified exhaustively: all 794 patterns within RS(12,8)'s distance recover byte-exactly, all 792 beyond it are refused, zero wrong bytes across 1,586 patterns.

Alongside these, closed forms govern deployment: constant-time index lookup, recovery linear in the post-checkpoint tail and independent of stored volume, steady-state write amplification `1/(1-u)`, and an optimal commit batch size `B* = sqrt(2·λ·A/c)` that landed within 5.35% of the measured optimum.

## Design highlights

- **Commit markers as the acknowledgement point.** A write is acknowledged when its commit marker is durable, not when the record lands — which is what makes the recovered state a prefix by construction. Batch size `b_commit` is the cost–durability dial.
- **Heapless `no_std` core** under `#![forbid(unsafe_code)]`, with no dynamic allocation anywhere. Every buffer is an owned array or a caller-supplied slice.
- **Async-native, blocking-projected.** Each algorithm is written once as an `async fn` over an `AsyncFlash` trait; the blocking API is a one-line projection per method. Zero-cost *when the flash driver never suspends* — see the limitations below.
- **Partial-key cuckoo index** with a fixed arena and a small stash: exactly `2b+s` = 16 slot probes per lookup, independent of load factor, at ~4.2 bytes per key.

## Known limitations

These are measured, not hypothetical. [`docs/specification.md`](docs/specification.md) records each one with the command and data file behind it:

- **Reclaimed space is not reusable.** The log head cannot yet wrap into freed segments, so a device halts with most of its segments free. This is a format-level gap and the most consequential item of remaining work.
- **RAM exceeds the documented budget.** The shipped ESP32 configuration needs ~81 KiB resident (~86 KiB at mount peak) against a 64 KiB target, dominated by a checkpoint buffer that must hold the entire serialised index. The largest configuration that fits 64 KiB is `n_buckets = 1024`.
- **Mount replay does not yield.** The recovery path is still on the blocking flash trait, so replaying 8,192 records is one uninterruptible span of ~4.6 s.
- **Sequential keys defeat the fingerprint.** Keys like `sensor_000123` drive the index collision rate to ~5.7× its theoretical bound; well-mixed keys stay below it.
- **Rollback protection needs a monotonic counter.** With a hardware counter the guarantee is as stated; with a flash-backed counter it is best-effort; with none it is absent. The engine reports which mode it is in rather than implying the strongest.

## Quickstart

### 1. Rust (`std`)

```toml
[dependencies]
slate-kv = "0.5"
```

```rust,ignore
use slate_kv::db::{Db, KeySource, Options, Profile};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `Options::default()` is a 4 MiB Pi-profile volume with b_commit = 8.
    let opts = Options {
        profile: Profile::Pi,
        ..Options::default()
    };

    // The root key is 32 bytes. Prefer KeySource::File or ::Env in production
    // so the key never appears in the binary.
    let mut db = Db::open(
        std::path::Path::new("./slate_db.bin"),
        KeySource::Bytes([0x42u8; 32]),
        opts,
    )?;

    db.put(b"sensor_1", b"23.5 C")?;

    // `put` buffers into the open batch; the write is acknowledged only once
    // `commit` makes the marker durable. `put_durable` does both in one call.
    db.commit()?;

    if let Some(val) = db.get(b"sensor_1")? {
        println!("sensor_1 = {}", String::from_utf8_lossy(&val));
    }

    // Rollback protection degrades honestly — check what you actually have.
    println!("security mode: {:?}", db.security_mode());
    Ok(())
}
```

### 2. C / C++ (`slate-kv-ffi`)

Build the library and header, then include [`slate.h`](crates/slate-kv-ffi/include/slate.h) and link `libslate_kv_ffi`:

```sh
make ffi-staticlib      # -> target/release/libslate_kv_ffi.a + include/slate.h
make ffi-native-libs    # prints the platform link flags you also need
```

```c
#include "slate.h"
#include <stdio.h>

int main(void) {
    uint8_t key[32] = {0x42};
    slate_options opts = {
        .capacity_bytes = 4u * 1024 * 1024,
        .max_keys       = 8192,
        .b_commit       = 8,
        .theta          = 0,     /* 0 = use the built-in default */
        .profile        = 1,     /* Pi */
    };

    struct slate_db *db = NULL;
    if (slate_open("./slate_db.bin", key, &opts, &db) != SLATE_OK) {
        /* The message is written into a caller-owned buffer. Pass NULL for the
           handle when open itself failed — there is no database to ask. */
        char err[256];
        slate_last_error_message(NULL, err, sizeof err);
        printf("open failed: %s\n", err);
        return 1;
    }

    slate_put(db, (const uint8_t *)"key1", 4, (const uint8_t *)"val1", 4);
    slate_commit(db);

    /* `vlen_inout` is in/out: set it to the buffer capacity on the way in,
       and it comes back as the value's true length. Passing NULL for the
       buffer turns this into a size query. */
    uint8_t buf[64];
    size_t out_len = sizeof buf;
    if (slate_get(db, (const uint8_t *)"key1", 4, buf, &out_len) == SLATE_OK) {
        printf("value: %.*s\n", (int)out_len, buf);
    }

    slate_close(db);
    return 0;
}
```

Check `slate_abi_version()` against the header's `SLATE_ABI_VERSION_MAJOR` before use, and treat `SLATE_ERR_TAMPERED` / `SLATE_ERR_ROLLBACK` as distinct security failures — never retry them.

### 3. Go (`bind/go`)

```sh
make bind-test          # builds the staticlib, runs the conformance suite
```

The Go binding is a thin cgo layer over the same C ABI; see [`bind/README.md`](bind/README.md) for the rules every binding must follow. It runs wherever Rust `std` runs — it is not a microcontroller path.

### 4. Bare-metal ESP32 firmware

```bash
cd targets/esp32
cargo build --release --bin kv_demo \
  --no-default-features --features chip-esp32c3,counter-flash,metrics \
  --target riscv32imc-unknown-none-elf
```

`counter-efuse` selects the hardware monotonic counter (full rollback protection); `counter-flash` keeps it in a flash sector (best-effort). `metrics` wires the write-amplification counters. The `scripts/` directory holds the QEMU crash suite and the Wokwi scenario used in CI.

## Repository layout

| Path | Contents |
|---|---|
| `crates/slate-kv-core` | Heapless `no_std` engine: log, index, epochs, GC, recovery |
| `crates/slate-kv` | `std` wrapper: `Db`, file-backed flash, POSIX durability |
| `crates/slate-kv-crypto` | AEAD sealer and key derivation |
| `crates/slate-kv-erasure` | Reed–Solomon over GF(2⁸) |
| `crates/slate-kv-hal` | Flash / counter traits and blocking↔async adapters |
| `crates/slate-kv-ffi` | Stable C ABI and generated `slate.h` |
| `crates/slate-kv-sim` | Simulated flash, crash injection, study harnesses |
| `crates/slate-kv-cli` | Command-line tool |
| `targets/esp32` | Bare-metal `esp-hal` firmware, QEMU and Wokwi harnesses |
| `bind/go` | Go / TinyGo binding over the C ABI |
| `docs/specification.md` | Normative format, semantics and conformance data |

## Development

```sh
make check-all          # fmt, lint, test, bare-metal build, FFI, bindings
cargo test --workspace  # 63 tests
```

CI additionally lints the out-of-workspace ESP32 firmware, runs the QEMU crash suite across three attack modes, and drives the Wokwi emulator scenario.

## Contributing

Contributions are welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md) before opening a PR or issue.

## License

Dual-licensed under either of:

- **MIT License** ([`LICENSE-MIT`](LICENSE-MIT))
- **Apache License, Version 2.0** ([`LICENSE-APACHE`](LICENSE-APACHE))

at your option.

---

