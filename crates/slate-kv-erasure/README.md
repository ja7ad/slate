# slate-kv-erasure

[![crates.io](https://img.shields.io/crates/v/slate-kv-erasure.svg)](https://crates.io/crates/slate-kv-erasure)
[![docs.rs](https://docs.rs/slate-kv-erasure/badge.svg)](https://docs.rs/slate-kv-erasure)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Reed–Solomon erasure coding for [SLATE](https://github.com/ja7ad/slate)'s bad-block tolerance.** Systematic RS(12, 8) over GF(2⁸): 8 data blocks plus 4 Cauchy parity blocks per stripe, any 4 of which can be lost and rebuilt exactly.

Heapless, `#![no_std]`, `#![forbid(unsafe_code)]`, **zero dependencies**. All buffers are caller-provided; the GF tables are `const` arrays in rodata.

Segment geometry and the parity layout are normative — see [`../../docs/specification.md`](../../docs/specification.md) § 2.3.

## Install

```sh
cargo add slate-kv-erasure
```

```toml
[dependencies]
slate-kv-erasure = "0.5"
```

## Why erasure coding and not ECC

NOR flash wears out in blocks. When a block goes bad the data in it is gone — but SLATE always *knows* which block failed, because every record carries an AEAD tag and a failed tag check localises the damage. That turns the general error-correction problem into the much cheaper **erasure** problem, where the failure positions are known, which is why the decoder here has no syndrome search and no Berlekamp–Massey step. It solves one 8×8 linear system in GF(2⁸) and back-substitutes.

This crate deliberately implements only the erasure case. **Undeclared corruption is not recoverable and not detected here:** if you flip bytes without declaring the affected blocks, `reconstruct` returns `Ok(())` having computed a wrong stripe. Measured over every 1- and 2-block corruption pattern, zero were recovered correctly. That is the defined behaviour of an erasure code, and it makes the guarantee **conditional on the driver telling you which blocks failed**. In SLATE the AEAD layer is the backstop — the wrong reconstruction fails to open, so wrong data does not reach the application — but this crate on its own cannot tell you anything is amiss.

If you need to correct errors at *unknown* positions, this is the wrong crate.

## Usage

```rust,ignore
use slate_kv_erasure::reconstruct::{reconstruct, BlockSet};
use slate_kv_erasure::{TooManyErasures, PAGE_SIZE, RS_K, RS_M, RS_N};

// A stripe is RS_K = 8 data pages followed by RS_M = 4 parity pages.
let mut stripe: [[u8; PAGE_SIZE]; RS_N] = load_stripe_from_flash();

// Declare what the AEAD tag checks localised as unreadable.
let mut erased = BlockSet::new();
erased.insert(2);   // data block 2 failed to verify
erased.insert(9);   // parity block 1 sits in a bad flash block
assert_eq!(erased.count(), 2);

match reconstruct(&mut stripe, &erased) {
    Ok(()) => {
        // stripe[2] and stripe[9] have been rebuilt in place, byte-exactly.
    }
    Err(TooManyErasures) => {
        // More than RS_M = 4 blocks lost: unrecoverable. Surface it.
    }
}
```

`reconstruct` repairs **in place** and allocates nothing — the stripe array you pass in is the only working memory, so a caller with 3 KiB of static RAM can repair a stripe. It rebuilds erased data blocks by inverting the survivor matrix, then re-encodes any erased parity blocks from the (now complete) data blocks. With zero declared erasures it returns `Ok(())` immediately.

There is no `encode` entry point in this crate. Parity is produced by declaring all `RS_M` parity indices erased and calling `reconstruct`, so encode and repair cannot drift apart.

> **The engine does not currently write RS parity.** `slate_kv_core::segment::encode_parity` implements seal-time encoding over the 8 data blocks, but it **has no caller anywhere in the workspace** — nothing seals a segment through it, so a live volume carries no RS parity blocks and this codec's protection is not yet in effect end to end. (The `parity_bytes` metric that `slate-kv` reports is the per-commit XOR head page, a different and much smaller mechanism.) The codec itself is complete and exhaustively verified, and the `slate-kv-sim` `rs_recovery` test exercises repair against injected bad blocks. Recorded as a gap in [`../../docs/specification.md`](../../docs/specification.md) §§ 5.3 and 8.8.

### Parameters

| Constant    | Value   | Meaning                                  |
|-------------|---------|------------------------------------------|
| `RS_K`      | 8       | data blocks per stripe                   |
| `RS_M`      | 4       | parity blocks per stripe                 |
| `RS_N`      | 12      | total blocks (`RS_K + RS_M`)             |
| `PAGE_SIZE` | 256     | bytes per block buffer                   |
| `GF_POLY`   | `0x11D` | field polynomial `x⁸ + x⁴ + x³ + x² + 1` |
| `GF_GEN`    | `0x02`  | generator for the log/exp tables         |

The shape is fixed at compile time by these constants rather than being generic, which is what keeps the working set static and the code predictable on a Cortex-M or RISC-V core. `m = 4` over `k = 8` costs **50% parity overhead**, which puts a floor of **1.5×** on write amplification for the data the parity covers, and buys tolerance of four simultaneous block failures per stripe. The encode cost never lands on the write hot path: parity is computed once, when a segment is *sealed*.

### Verified behaviour

Every erasure pattern of every size was tested, not a sample — `C(12, e)` patterns for each `e`:

| Blocks lost |  Patterns | Recovered exactly | Refused | Wrong bytes |
|------------:|----------:|------------------:|--------:|------------:|
|         0–4 |       794 |               794 |       0 |           0 |
|           5 |       792 |                 0 |     792 |           0 |
|   **Total** | **1,586** |           **794** | **792** |       **0** |

All 794 patterns within the code distance reconstructed byte-exactly; all 792 beyond it were refused with an explicit error; no survivor matrix was singular; zero wrong bytes across all 1,586 patterns. The harness checks correctness at the cryptographic layer, not just the byte layer: it requires all 12 blocks byte-identical *and* all 27 packed SLATE records re-openable under AEAD. Reproduction command and data file: [`../../docs/specification.md`](../../docs/specification.md) § 6.5.

## Module map

| Module | Contents |
|---|---|
| `gf` | GF(2⁸) arithmetic: `GF_EXP` (512 B) and `GF_LOG` (256 B) tables built in `const` context, plus `gf_mul` and `gf_inv` |
| `matrix` | `cauchy_row(j)` for the systematic generator, `gf_matrix_invert` (Gauss–Jordan in GF(2⁸)) |
| `reconstruct` | `BlockSet` — a `u16` bitset with `new`/`insert`/`contains`/`count` — and `reconstruct` |

Errors are two distinct zero-sized types, so a caller cannot confuse them: `TooManyErasures` (more than `RS_M` blocks lost, a real data-loss event) and `Singular` (returned by `gf_matrix_invert` when a matrix is not invertible, which the Cauchy construction makes unreachable for survivor matrices — `reconstruct` maps it to `TooManyErasures`).

`BlockSet::insert` and `contains` silently ignore indices at or above `RS_N` rather than panicking, so an out-of-range index is dropped, not caught.

## Using it outside SLATE

Nothing here depends on the rest of the workspace — no `slate-kv-*` imports and no external crates at all. If you want a small, allocation-free, table-driven RS(12,8) erasure codec for an embedded project, this crate stands alone. What you give up relative to a general RS library is configurability: `k`, `m` and the block size are compile-time constants, and there is no error-correction (unknown-position) mode.

## Testing

```sh
cargo test -p slate-kv-erasure                        # GF field laws, matrix inversion, round-trips
cargo run --release -p slate-kv-sim --example rs_exhaustive   # the exhaustive table above
cargo test -p slate-kv-sim --test rs_recovery         # end-to-end repair against injected bad blocks
```

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
