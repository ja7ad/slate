# slate-kv-crypto

[![crates.io](https://img.shields.io/crates/v/slate-kv-crypto.svg)](https://crates.io/crates/slate-kv-crypto)
[![docs.rs](https://docs.rs/slate-kv-crypto/badge.svg)](https://docs.rs/slate-kv-crypto)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**[SLATE](https://github.com/ja7ad/slate)'s cryptographic layer.** The key hierarchy, and the one implementation of the `Sealer` trait the engine calls to seal and open records, commit markers and checkpoints.

`#![no_std]`, `#![forbid(unsafe_code)]`, no heap, no runtime randomness. Built on RustCrypto: ChaCha20-Poly1305, HKDF-SHA-256, HMAC-SHA-256.

Nonce construction, the key schedule and the authenticated-data layout are normative — see [`../../docs/specification.md`](../../docs/specification.md) §§ 2.4–2.8.

## Install

```sh
cargo add slate-kv-crypto
```

```toml
[dependencies]
slate-kv-crypto = "0.5"
```

Most users never depend on this crate directly — [`slate-kv`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv) wires it up for you. Reach for it when you are building your own engine host (bare-metal firmware, a custom backend) and have to construct the `Sealer` yourself.

## Usage

```rust,ignore
use slate_kv_crypto::keys::{DeviceKey, KeySet};
use slate_kv_crypto::sealer::CryptoSealer;

// The 32-byte root key: from eFuse, an OS keystore, or a provisioning step.
let device_key = DeviceKey(load_root_key());

// Derive the subkey set for the epoch you are mounting.
let keys = KeySet::derive(&device_key, epoch);

// This is what `slate_kv_core::slate::Slate` takes as its `S: Sealer`.
let sealer = CryptoSealer::new(keys);
```

`CryptoSealer::new` consumes the `KeySet`, so the sealer owns the only copy and it is zeroized when the sealer drops. At each epoch boundary the engine calls `Sealer::roll_epoch(e)`, which re-derives the record subkey; you do not call it yourself.

## Key hierarchy

One root key `K` never leaves RAM and never touches flash. Everything else is derived from it with domain separation, so compromising one subkey does not cross into another use:

```text
K  (32 bytes, DeviceKey)
└── prk = HKDF-Extract(salt = "SLATE/v1", ikm = K)
    ├── k_cm    = HKDF-Expand(prk, "cm")               commit-marker MAC
    ├── k_ckpt  = HKDF-Expand(prk, "ckpt")             checkpoint AEAD
    ├── k_ctr   = HKDF-Expand(prk, "ctr")              monotonic-counter MAC
    └── k_rec_e = HKDF-Expand(prk, "rec" ‖ le64(e))    per-epoch record AEAD
```

`KeySet` holds `prk`, the three fixed subkeys, `k_rec_e` for the **current** epoch, and that epoch number. `roll_epoch(e)` overwrites `k_rec_e` with one HKDF-Expand call.

Because the KDF is deterministic, an older epoch's record key is always re-derivable from `prk` and never has to be stored — that is what `KeySet::derive_rec_key(e, &mut out)` is for, and it is the reason records written before a rotation stay readable without the log being rewritten.

Both `DeviceKey` and `KeySet` derive `ZeroizeOnDrop` and have hand-written `Debug` impls printing `<REDACTED>`, so a stray `dbg!` or log line cannot leak key material.

## Nonces are derived, never random

```rust,ignore
// Re-exported from slate_kv_core::record so there is exactly one definition.
pub fn record_nonce(seq: u64, epoch: u32) -> [u8; 12]   // le64(seq) ‖ le32(epoch)
```

The 96-bit AEAD nonce is the record's sequence number in the low 8 bytes and an **epoch discriminator** in the high 4. `seq` alone already guarantees uniqueness — it is a strictly increasing total order that is never reset — so the epoch field is not there for uniqueness. It is there so a reader can tell which `k_rec_e` opens a given record, and it costs no extra header bytes because the nonce is already in the header.

Deriving the nonce is what lets the engine need no runtime randomness at all: no RNG on the hot path, no entropy pool to seed on a microcontroller, and a nonce whose uniqueness is auditable by inspection.

The high epoch bytes are read back with `record::nonce_epoch(&nonce)` or `record::hdr_epoch(&hdr)`, and `seal_epoch` refuses to roll past `record::MAX_REC_EPOCH` = `u32::MAX` rather than silently aliasing epoch keys.

The safety of this rests on two invariants the engine upholds and **any custom host must also uphold**:

1. **Single writer.** `seq` is produced by one logical writer. Two writers sharing a key would reuse nonces and lose all confidentiality guarantees.
2. **The epoch in the header is authenticated.** `CryptoSealer` selects the record key from the epoch stamped in the header, on both the seal and the open path — so the two are symmetric. Because the header is the associated data, forging that discriminator to steer key selection breaks the tag first: it surfaces as `Error::Tampered`, never as wrong plaintext.

## What gets authenticated

| Object        | Primitive         | Key                            | Nonce / associated data                                                         |
|---------------|-------------------|--------------------------------|---------------------------------------------------------------------------------|
| Record        | ChaCha20-Poly1305 | `k_rec_e`, `e` from the header | nonce from the header; the full 28-byte header is the AD                        |
| Commit marker | HMAC-SHA-256      | `k_cm`                         | MAC over the marker's first 51 bytes: `magic ‖ seq_max ‖ epoch ‖ xor_pages ‖ χ` |
| Checkpoint    | ChaCha20-Poly1305 | `k_ckpt`                       | nonce is `le64(epoch) ‖ slot`; caller-supplied AD                               |
| Counter slot  | HMAC-SHA-256      | `k_ctr`                        | derived here, used by the host's counter implementation                         |

The record header is bound as associated data but left in the clear, so the engine can read `seq`, `op`, the fingerprint and the lengths to walk the log without a key — and cannot alter any of them without invalidating the tag.

**Keys and values live inside the ciphertext.** One record seals `k ‖ v` as a single plaintext; the on-flash header carries only a 16-bit fingerprint of the key. An attacker with the flash image learns record sizes and timing, not key names.

A commit marker is `CM_LEN` = 83 bytes: 51 bytes of authenticated fields followed by the 32-byte HMAC tag. `verify_marker` takes `&self`, checks the MAC before parsing, and returns the decoded `CmFields`.

## The `Sealer` seam

`Sealer` is defined in [`slate-kv-core`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv-core)'s `log` module, not here — the engine depends on the trait and this crate provides the implementation. That inversion is what lets a target with a crypto accelerator swap in a hardware-backed `Sealer` without the engine noticing.

If you write your own, the contract is:

| Method                                                     | Obligation                                                                                                                                     |
|------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------|
| `seal_record(&mut self, hdr, plain_kv, ct_tag_out)`        | AEAD over `k ‖ v` with the 28-byte header as AD. Writes ciphertext then the 16-byte tag into `ct_tag_out`.                                     |
| `open_record(&mut self, hdr, ct_tag, plain_out)`           | Verify **before** writing plaintext; on failure zeroize `plain_out` and return `Error::Tampered`.                                              |
| `commit_marker(&mut self, seq_max, epoch, xor_pages, chi)` | Produce the `[u8; CM_LEN]` marker.                                                                                                             |
| `verify_marker(&self, cm)`                                 | Constant-time tag comparison; `Err(Error::Tampered)` on failure, `Err(Error::FormatError)` on a bad magic byte. Note `&self`, not `&mut self`. |
| `seal_checkpoint(&mut self, epoch, slot, ad, in_out)`      | In-place AEAD over the index snapshot, returning the 16-byte tag.                                                                              |
| `open_checkpoint(&mut self, epoch, slot, ad, in_out, tag)` | In-place open; zeroize `in_out` and return `Error::Tampered` on failure.                                                                       |
| `roll_epoch(&mut self, e)`                                 | Re-derive the record subkey for epoch `e`.                                                                                                     |

Every `open_*` must verify the tag **before** returning any plaintext, and must map a tag failure to `Error::Tampered` — never to a generic I/O error. Tamper detection flattened into "something went wrong" is tamper detection nobody acts on.

## Algorithm choices

ChaCha20-Poly1305 is the portable default: constant-time in software on cores with no AES instructions, which is exactly the ESP32 and Cortex-M case. AES-256-GCM behind a feature flag for hardware-accelerated targets is designed for but **not implemented** — there is no such feature in this crate today.

Also not present: any key-wrapping, key-rotation-of-`K`, or attestation facility. Rotating the device key means re-encrypting the store, which nothing here does for you.

## Testing

```sh
cargo test -p slate-kv-crypto        # seal/open round-trips, cross-epoch reads, tamper cases
cargo test -p slate-kv --test security
```

The tamper tests are the interesting ones: they assert that a flipped ciphertext byte, a flipped header byte, a forged epoch discriminator and a flipped marker byte each produce `Error::Tampered` with the output buffer left zeroized.

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
