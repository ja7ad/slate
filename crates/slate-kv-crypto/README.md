# slate-kv-crypto

[![crates.io](https://img.shields.io/crates/v/slate-kv-crypto.svg)](https://crates.io/crates/slate-kv-crypto)
[![docs.rs](https://docs.rs/slate-kv-crypto/badge.svg)](https://docs.rs/slate-kv-crypto)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**[SLATE](https://github.com/ja7ad/slate)'s cryptographic layer (L3).** The key hierarchy, and the one implementation of the `Sealer` trait the engine calls to seal and open records, commit markers, and checkpoints.

`#![no_std]`, `#![forbid(unsafe_code)]`, no heap. Built on RustCrypto: ChaCha20-Poly1305, HKDF-SHA-256, HMAC-SHA-256.

## Install

```sh
cargo add slate-kv-crypto
```

```toml
[dependencies]
slate-kv-crypto = "0.4"
```

Most users never depend on this crate directly — [`slate-kv`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv) wires it up for you. Depend on it when you are building your own engine host (bare-metal firmware, a custom backend) and need to construct the `Sealer` yourself.

## Usage

```rust
use slate_kv_crypto::keys::{DeviceKey, KeySet};
use slate_kv_crypto::sealer::CryptoSealer;

// The 32-byte root key: from eFuse, an OS keystore, or a caller-provided secret.
let device_key = DeviceKey(load_root_key());

// Derive the subkey set for the epoch you are mounting.
let keys = KeySet::derive(&device_key, epoch);

// This is what `slate_kv_core::slate::Slate` takes as its `S: Sealer`.
let sealer = CryptoSealer::new(keys);
```

At each epoch boundary the engine calls `Sealer::roll_epoch(e)`, which re-derives the record subkey and resets the nonce space. You do not call it yourself.

## Key hierarchy

One root key `K` never leaves RAM and never touches flash. Everything else is derived from it with domain separation, so a compromise of one subkey does not cross into another use:

```text
K  (32 bytes, DeviceKey)
└── prk = HKDF-Extract(salt = "SLATE/v1", ikm = K)
    ├── k_cm    = HKDF-Expand(prk, "cm")               commit-marker MAC
    ├── k_ckpt  = HKDF-Expand(prk, "ckpt")             checkpoint AEAD
    ├── k_ctr   = HKDF-Expand(prk, "ctr")              monotonic-counter MAC
    └── k_rec_e = HKDF-Expand(prk, "rec" ‖ le64(e))    per-epoch record AEAD
```

Only the **current** epoch's record key is kept in RAM (`KeySet::k_rec_e`); `roll_epoch(e)` overwrites it with one HKDF-Expand call. Both `DeviceKey` and `KeySet` derive `ZeroizeOnDrop` and have hand-written `Debug` impls that print `<REDACTED>`, so a stray `dbg!` or a log line cannot leak key material.

## Nonces are derived, never random

```rust
pub fn record_nonce(seq: u64) -> [u8; 12]  // le64(seq) ‖ 0u32
```

The 96-bit AEAD nonce is the record's sequence number, zero-extended. This is deliberate and it is what makes the core need no runtime randomness at all — no RNG on the hot path, no entropy pool to seed on a microcontroller, and a nonce that is trivially auditable for uniqueness.

The safety of this rests on two invariants the engine upholds, and that **any custom host must also uphold**:

1. **Single writer.** `seq` is a strictly increasing total order produced by one logical writer. Two writers sharing one key would reuse nonces and lose all confidentiality guarantees.
2. **Fresh subkey per epoch.** `seq` restarts within an epoch's nonce space, but `k_rec_e` is fresh per epoch, so the (key, nonce) pair is still never repeated.

## What gets authenticated

| Object | Primitive | Key | Associated data |
|---|---|---|---|
| Record | ChaCha20-Poly1305 | `k_rec_e` | the full 28-byte record header |
| Commit marker | HMAC-SHA-256 | `k_cm` | `seq_max ‖ epoch ‖ xor_pages ‖ χ` |
| Checkpoint | ChaCha20-Poly1305 | `k_ckpt` | epoch, slot, caller AD |
| Counter slot | HMAC-SHA-256 | `k_ctr` | counter value |

The record header is bound as associated data, not encrypted, so the engine can read `seq`, `op`, `h(k)`, and the lengths to walk the log without a key — but cannot alter any of them without invalidating the tag.

**Keys and values live inside the ciphertext.** A record seals `k ‖ v` as one plaintext; the on-flash header carries only a truncated hash of the key. An attacker with the flash image learns record sizes and timing, not key names.

## The `Sealer` seam

`Sealer` is defined in [`slate-kv-core`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv-core), not here — the engine depends on the trait, and this crate provides the implementation. That inversion is what lets a target with a crypto accelerator swap in a hardware-backed `Sealer` without the engine noticing.

If you write your own, the contract is:

- `seal_record` / `open_record` — AEAD over `k ‖ v` with the header as AD.
- `commit_marker` / `verify_marker` — produce and check the `CM_LEN`-byte marker. `verify_marker` takes `&self` and must be constant-time in the tag comparison.
- `seal_checkpoint` / `open_checkpoint` — in-place AEAD over the index snapshot, returning/taking the 16-byte tag.
- `roll_epoch(e)` — re-derive the record subkey.

Every `open_*` must verify the tag **before** returning any plaintext to the caller, and must map a tag failure to `Error::Tampered` — never to a generic I/O error. Tamper detection that gets flattened into "something went wrong" is tamper detection that nobody acts on.

## Algorithm choices

ChaCha20-Poly1305 is the portable default: constant-time in software on cores with no AES instructions, which is exactly the ESP32/Cortex-M case. AES-256-GCM behind a feature flag for hardware-accelerated targets is designed for but not yet implemented.

## Report references

Key hierarchy and nonce construction §3.3 · confidentiality (G1) and integrity (G2) reductions §6 · checkpoint sealing §3.6. See [`docs/SLATE_FORMAL_SPECIFICATION.md`](https://github.com/ja7ad/slate/blob/main/docs/SLATE_FORMAL_SPECIFICATION.md).

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
