# SLATE: Secure, Log-structured, Authenticated, Tamper-Evident Key–Value Engine

[![CI](https://github.com/javad/slate/actions/workflows/ci.yml/badge.svg)](https://github.com/javad/slate/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)
[![no_std](https://img.shields.io/badge/rust-no__std-green.svg)](crates/slate-core)

**SLATE** (*Secure, Log-structured, Authenticated, Tamper-Evident*) is a single-device key–value (KV) storage engine designed from the ground up for the edge computing regime—from bare-metal microcontrollers like the ESP32 to single-board computers like the Raspberry Pi.

SLATE provides a mathematically rigorous foundation that balances four simultaneous, often conflicting, objectives: an **ultra-light memory footprint**, **high performance**, **low energy consumption**, and **strong at-rest security**. Rather than claiming an impossible Pareto-dominating point, SLATE delivers a formally specified composition of well-understood primitives whose guarantees are mathematically proven.

---

## Key Features

- ⚡ **Freshness-Bound $O(1)$ Authenticated Append-Log:** Offers whole-store tamper-evidence and epoch-granular hardware monotonic counter rollback protection. Features constant-time chain updates and $O(1)$ freshness-tip verification on boot (G1–G3).
- 🔋 **Energy-Optimal Commit Scheduling:** Utilizes an Economic-Order-Quantity (EOQ) style dynamic integer-only scheduler ($B^\star$) for durable commits, optimizing the trade-off between retention latency and the fixed energy cost of waking the flash (Theorem 9).
- 🧠 **Ultra-Light RAM Index (`no_std`)**: Operates completely allocation-free with a bounded, compile-time asserted RAM footprint ($\le 32\text{--}64\text{ KB}$). Employs a partial-key cuckoo hash index ensuring worst-case $O(1)$ lookup with a load-factor guarantee.
- 🛡️ **Proven Prefix-Durability**: Guaranteed zero acknowledged write loss across arbitrary power failures (Theorem 1). Recovery bounds logical reconstruction to a constant $O(\Theta)$ replay from the last checkpoint.
- 🧩 **Bad-Block Tolerance**: Integrates systematic Reed–Solomon $\mathrm{RS}(n,k)$ erasure coding over $\mathrm{GF}(2^8)$ and per-batch XOR parity to protect both sealed segments and the open head segment against flash bit-rot without hot-path write overhead (Theorem 8).
- 🌐 **Multi-Target Architecture**: Cleanly separated into a heapless `no_std` core, `std` POSIX wrapper, C ABI FFI, and bare-metal `esp-hal` firmware for ESP32.

---

## Workspace Layout

```
slate/
├── Cargo.toml                  # Workspace manifest (MIT OR Apache-2.0)
├── crates/
│   ├── slate-core/             # Heapless engine core (no_std, zero alloc)
│   ├── slate-crypto/           # AEAD, KDF key hierarchy, MAC sealer (no_std)
│   ├── slate-erasure/          # Reed–Solomon RS(n,k) erasure coder (no_std)
│   ├── slate-hal/              # Hardware Abstraction Layer traits (no_std)
│   ├── slate/                  # std wrapper & POSIX FileFlash engine
│   ├── slate-cli/              # CLI binary utility (put, get, del, stats)
│   ├── slate-ffi/              # C ABI bindings (cbindgen header generation)
│   └── slate-sim/              # Deterministic crash-injection simulator
├── targets/
│   └── esp32/                  # Bare-metal esp-hal port (ESP32-C3 QEMU & Wokwi)
└── docs/
    ├── SLATE_FORMAL_SPECIFICATION.md  # Formal mathematical report & theorems
    └── slate_qemu_benchmarks.md       # Empirical benchmark results
```

---

## Quickstart

### 1. Using `slate` in Rust (`std`)

Add `slate` to your `Cargo.toml`:
```toml
[dependencies]
slate = "0.1"
```

Basic Put / Get usage:
```rust,ignore
use slate::Db;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open or create a database using file-backed flash emulation
    let mut db = Db::open("./slate_db.bin", [0x42u8; 32])?;

    // Put key-value pair
    db.put(b"sensor_1", b"23.5 C")?;
    db.commit()?; // Flush batch to disk

    // Get value
    if let Some(val) = db.get(b"sensor_1")? {
        println!("sensor_1 = {}", String::from_utf8_lossy(&val));
    }

    Ok(())
}
```

### 2. Using `slate-ffi` in C / C++

Include [`slate.h`](crates/slate-ffi/include/slate.h) and link against `libslate_ffi`:

```c
#include "slate.h"
#include <stdio.h>

int main(void) {
    uint8_t key[32] = {0};
    slate_db_t *db = slate_open("./slate_db.bin", key);
    if (!db) {
        printf("Failed to open SLATE database\n");
        return 1;
    }

    slate_put(db, (const uint8_t*)"key1", 4, (const uint8_t*)"val1", 4);
    slate_commit(db);

    uint8_t buf[64];
    size_t out_len = 0;
    if (slate_get(db, (const uint8_t*)"key1", 4, buf, sizeof(buf), &out_len) == SLATE_OK) {
        printf("Value: %.*s\n", (int)out_len, buf);
    }

    slate_close(db);
    return 0;
}
```

### 3. Bare-Metal `no_std` Firmware (ESP32)

Building firmware for ESP32-C3:
```bash
cd targets/esp32
cargo build --release --bin kv_demo --target riscv32imc-unknown-none-elf
```

---

## Formal Specification & Benchmarks

- **Formal Mathematical Specification**: See [`docs/SLATE_FORMAL_SPECIFICATION.md`](docs/SLATE_FORMAL_SPECIFICATION.md) for formal proofs of prefix-durability, index reconstructibility, security reductions, and cost models.
- **Empirical QEMU Benchmarks**: See [`docs/slate_qemu_benchmarks.md`](docs/slate_qemu_benchmarks.md) for crash Monte-Carlo results, write-amplification under Zipf skew, and energy decay sweeps.

> **Honesty Note**: If deploying to a high-throughput desktop OS server where active tamper-resistance and deterministic low-RAM footprint are not required, standard engines (e.g., RocksDB or SQLite) may provide higher raw I/O throughput. SLATE is specifically engineered for edge environments requiring tamper-evidence, crash-safety, and tight RAM budgets.

---

## Contributing

We welcome contributions! Please review our [`CONTRIBUTING.md`](CONTRIBUTING.md) guide before submitting pull requests or issues.

---

## License

Dual-licensed under either of:

- **MIT License** ([`LICENSE-MIT`](LICENSE-MIT))
- **Apache License, Version 2.0** ([`LICENSE-APACHE`](LICENSE-APACHE))

at your option.

---

## Citation

If you use SLATE or reference its formal specification, correctness proofs, or energy models in your research or system design, please cite:

```bibtex
@techreport{slate2026formal,
  title       = {SLATE: A Provably Secure, Ultra-Light, Low-Power Key--Value Engine for Edge Devices},
  subtitle    = {Formal model, correctness and security theorems, cost models, and a Pareto-optimal operating point},
  author      = {SLATE Technical Team},
  year        = {2026},
  institution = {SLATE Project},
  note        = {Available at docs/SLATE_FORMAL_SPECIFICATION.md}
}
```

