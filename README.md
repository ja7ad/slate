# SLATE: Secure, Log-structured, Authenticated, Tamper-Evident Key–Value Engine

<p align="center">
  <img src="https://raw.githubusercontent.com/ja7ad/slate/main/docs/slate_logo.png" alt="SLATE Logo" width="300"/>
</p>

[![codecov](https://codecov.io/gh/ja7ad/slate/graph/badge.svg?token=leYIzSiuLf)](https://codecov.io/gh/ja7ad/slate)
[![CI](https://github.com/ja7ad/slate/actions/workflows/ci.yml/badge.svg)](https://github.com/ja7ad/slate/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)
[![no_std](https://img.shields.io/badge/rust-no__std-green.svg)](crates/slate-kv-core)

SLATE is a single-device key-value engine for edge computing, from bare-metal microcontrollers like the ESP32 up to boards like the Raspberry Pi. It's built around four goals that usually fight each other: a tiny memory footprint, good performance, low energy use, and real at-rest security.

We don't claim to beat every engine on every axis at once — that's not possible. Instead SLATE composes well-understood primitives (log-structured storage, AEAD, cuckoo hashing, Reed-Solomon parity) into a design whose guarantees are proven rather than assumed, and picks concrete operating points on the resulting trade-off curve.

## Key features

- **Freshness-bound O(1) authenticated log** — whole-store tamper-evidence plus epoch-granular rollback protection via a hardware monotonic counter. Chain updates and boot-time freshness checks are both O(1) (G1–G3 in the formal spec).
- **Energy-optimal commit scheduling** — an EOQ-style scheduler picks the commit batch size $B^\star$ that minimizes energy per operation, balancing flash wake-up cost against write latency.
- **Ultra-light RAM index (`no_std`)** — a partial-key cuckoo hash index with zero heap allocation and a compile-time-bounded footprint (roughly 32–64 KB), giving worst-case O(1) lookups.
- **Proven prefix-durability** — no acknowledged write is ever lost across arbitrary power failures, and recovery after a crash is bounded to a constant O(Θ) replay from the last checkpoint.
- **Bad-block tolerance** — Reed–Solomon RS(n,k) erasure coding over GF(2⁸) plus per-batch XOR parity protect both sealed segments and the open head segment, without adding overhead to the write hot path.
- **Runs everywhere** — a heapless `no_std` core, a `std` wrapper for POSIX systems, a C ABI, and a bare-metal `esp-hal` firmware target for the ESP32.

## Quickstart

### 1. Rust (`std`)

```toml
[dependencies]
slate-kv = "0.4"
```

```rust,ignore
use slate_kv::Db;

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

### 2. C / C++ (`slate-kv-ffi`)

Include [`slate.h`](crates/slate-kv-ffi/include/slate.h) and link against `libslate_kv_ffi`:

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

### 3. Bare-metal ESP32 firmware

```bash
cd targets/esp32
cargo build --release --bin kv_demo --target riscv32imc-unknown-none-elf
```

## Contributing

Contributions are welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md) before opening a PR or issue.

## License

Dual-licensed under either of:

- **MIT License** ([`LICENSE-MIT`](LICENSE-MIT))
- **Apache License, Version 2.0** ([`LICENSE-APACHE`](LICENSE-APACHE))

at your option.

---

