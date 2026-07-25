# slate-kv-erasure

[![crates.io](https://img.shields.io/crates/v/slate-kv-erasure.svg)](https://crates.io/crates/slate-kv-erasure)
[![docs.rs](https://docs.rs/slate-kv-erasure/badge.svg)](https://docs.rs/slate-kv-erasure)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Reed–Solomon erasure coding for [SLATE](https://github.com/ja7ad/slate)'s bad-block tolerance (layer L5).** Systematic RS(12, 8) over GF(2⁸): 8 data blocks + 4 parity blocks per stripe, any 4 of which may be lost and rebuilt.

Heapless, `#![no_std]`, `#![forbid(unsafe_code)]`, zero dependencies. All buffers are caller-provided; the GF tables are two 512-byte `const` arrays that live in rodata.

## Install

```sh
cargo add slate-kv-erasure
```

```toml
[dependencies]
slate-kv-erasure = "0.3"
```

## Why erasure coding and not ECC

NOR flash wears out in blocks. When a block goes bad, the data in it is gone — but SLATE always *knows* which block failed, because every record carries an AEAD tag: a failed tag check localises the damage. That turns the general error-correction problem into the much cheaper **erasure** problem (known failure positions), which is why the decoder here has no syndrome search or Berlekamp–Massey step. It solves one 8×8 linear system in GF(2⁸) and back-substitutes.

This crate deliberately implements only the erasure case. If you need to correct errors at *unknown* positions, this is the wrong crate.

## Usage

### Reconstructing a damaged stripe

```rust
use slate_kv_erasure::reconstruct::{reconstruct, BlockSet};
use slate_kv_erasure::{PAGE_SIZE, RS_N};

// A full stripe: 8 data pages followed by 4 parity pages.
let mut stripe: [[u8; PAGE_SIZE]; RS_N] = load_stripe_from_flash();

// Mark what the AEAD tag checks told us is unreadable.
let mut erased = BlockSet::new();
erased.insert(2);   // data block 2 failed to verify
erased.insert(9);   // parity block 1 is in a bad flash block

match reconstruct(&mut stripe, &erased) {
    Ok(()) => {
        // stripe[2] and stripe[9] have been rebuilt in place.
    }
    Err(slate_kv_erasure::TooManyErasures) => {
        // More than RS_M = 4 blocks lost: unrecoverable, surface it.
    }
}
```

`reconstruct` repairs **in place** and allocates nothing — the stripe array you pass in is the only working memory, so a caller with 3 KiB of static RAM can repair a stripe.

### Parameters

| Constant | Value | Meaning |
|---|---|---|
| `RS_K` | 8 | data blocks per stripe |
| `RS_M` | 4 | parity blocks per stripe |
| `RS_N` | 12 | total blocks (`RS_K + RS_M`) |
| `PAGE_SIZE` | 256 | bytes per block buffer |
| `GF_POLY` | `0x11D` | field polynomial `x⁸ + x⁴ + x³ + x² + 1` |
| `GF_GEN` | `0x02` | generator for the log/exp tables |

The m = 4 over k = 8 pick is the max-parity Pareto point from report §9: 50 % parity overhead buys tolerance of four simultaneous block failures per stripe, and the encode cost never lands on the write hot path (parity is computed once, when a segment is *sealed*).

## Module map

| Module | Contents |
|---|---|
| `gf` | GF(2⁸) arithmetic: `GF_EXP` / `GF_LOG` tables built at compile time, `gf_mul`, `gf_inv` |
| `matrix` | `cauchy_row(j)` for the systematic generator, `gf_matrix_invert` (Gauss–Jordan in GF(2⁸)) |
| `reconstruct` | `BlockSet` (a `u16` bitset of erased indices) and `reconstruct` |

Errors are two distinct zero-sized types, so a caller cannot confuse them: `TooManyErasures` (more than `RS_M` blocks lost — a real data-loss event) and `Singular` (the survivor matrix was not invertible, which the Cauchy construction makes unreachable).

## Using it outside SLATE

Nothing here depends on the rest of the workspace — no `slate-kv-*` imports at all. If you want a small, allocation-free, table-driven RS(12,8) erasure codec for an embedded project, you can use this crate on its own. The shape is fixed at compile time by the `RS_K` / `RS_M` / `PAGE_SIZE` constants rather than being generic, which is what keeps the working set static and the code branch-free enough for a Cortex-M or RISC-V core.

## Testing

```sh
cargo test -p slate-kv-erasure          # GF field laws, matrix inversion, round-trips
cargo test -p slate-kv-sim rs_recovery  # end-to-end repair against injected bad blocks
```

## Report references

Erasure model and RS(n,k) choice §7 · parity/lifetime trade-off and the max-parity operating point §9. See [`docs/SLATE_FORMAL_SPECIFICATION.md`](https://github.com/ja7ad/slate/blob/main/docs/SLATE_FORMAL_SPECIFICATION.md).

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
