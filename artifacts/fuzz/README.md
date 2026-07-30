# slate-fuzz

[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Fuzz targets for [SLATE](https://github.com/ja7ad/slate)'s attack surface.** Everything here feeds adversary-controlled bytes to the code that parses flash.

This is the surface that matters: an at-rest attacker cannot make the engine call an API, but they can rewrite every byte of the medium the decoders read. A decoder that panics on a crafted header turns a tamper attempt into a denial of service; one that reads past a length field turns it into something worse.

## Prerequisites

```sh
cargo install cargo-fuzz     # requires a nightly toolchain (libFuzzer)
```

## Run

```sh
cd artifacts/fuzz
cargo +nightly fuzz run fuzz_record_decode
cargo +nightly fuzz run fuzz_record_decode -- -max_total_time=300   # time-boxed, for CI
cargo +nightly fuzz list                                            # all targets
```

A crash writes a reproducer to `artifacts/<target>/`; replay it with:

```sh
cargo +nightly fuzz run fuzz_record_decode artifacts/fuzz_record_decode/crash-<hash>
```

## Targets

| Target | Feeds | Status |
|---|---|---|
| `fuzz_record_decode` | Arbitrary 28-byte buffers into `RecordHeader::decode` | **Implemented** |
| `fuzz_ckpt_decode` | Arbitrary 44-byte buffers into `CheckpointHeader::decode` | **Implemented** |
| `fuzz_marker_decode` | Commit-marker parsing | **Empty stub** |
| `fuzz_recover` | Arbitrary flash images into `mount` / `recover` | **Empty stub** |

The two stubs compile and run but assert nothing — do not read a clean run of them as coverage. `fuzz_recover` is the valuable one still to be written: it is the only target that exercises the decoders in composition rather than in isolation.

## The property being fuzzed

Every decoder must be **total over arbitrary input**: for any byte string, return a value or a typed error, never panic, never loop unboundedly, never index out of bounds. In `slate-kv-core` that means:

- `Err(Error::FormatError)` for a structurally invalid buffer (bad magic, impossible lengths),
- `Err(Error::Tampered)` for a structurally valid buffer whose authentication fails,
- and for `recover`, one of `Ok(_)`, `Tampered`, `Rollback`, or `TornTail` — a wrong *value* is a bug, but a *panic* is the bug class fuzzing exists to find.

`slate-kv-core` is `#![forbid(unsafe_code)]`, so a memory-safety violation is not the concern; a panic in `no_std` firmware is, because it usually means a reboot loop on a device nobody can reach.

## Writing a new target

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = slate_kv_core::some::decode(data);   // assert nothing; a panic is the failure
});
```

Then register it in `Cargo.toml`:

```toml
[[bin]]
name = "fuzz_my_target"
path = "src/bin/fuzz_my_target.rs"
test = false
doc = false
```

Prefer feeding the raw slice over reconstructing a "plausible" input — the interesting inputs are the ones a fixed-size copy would reject before the decoder ever sees them. Where a target needs structure (a whole flash image), use `arbitrary` to derive it rather than hand-rolling a parser inside the harness.

Seed the corpus from real data when you have it: `corpus/<target>/` picks up any files you drop in, and a genuine `data.bin` from a test database is a far better starting point than random bytes.

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
