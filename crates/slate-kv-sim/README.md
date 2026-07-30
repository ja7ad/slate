# slate-kv-sim

[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Where [SLATE](https://github.com/ja7ad/slate)'s claims become tests.** A deterministic in-RAM flash with fault injection — power loss at an arbitrary byte, bad blocks, program-once violations — plus the property tests and measurement harnesses that run the engine against it.

**Development-only.** Not published to crates.io (`publish = false`) and it has no stable API. It exists so that "prefix durability holds under arbitrary power loss" is something CI checks rather than something a document asserts.

The measurements produced here, with their reproduction commands and data files, are collected in [`../../docs/specification.md`](../../docs/specification.md) § 6.

## Run it

```sh
cargo test -p slate-kv-sim                       # everything
cargo test -p slate-kv-sim --test rs_recovery    # one suite
```

## Why simulate flash at all

Crash-safety bugs are the ones you cannot find by testing on real hardware: they need a power cut at one specific byte of one specific program operation, and you cannot arrange that on demand. `SimFlash` makes the crash point a parameter.

```rust,ignore
use slate_kv_sim::{Crash, SimFlash};

let mut flash = SimFlash::new(1024 * 1024, 256, 4096);   // capacity, page, block

// Cut power partway through the 42nd flash operation, 17 bytes in.
flash.power.crash = Crash::AtByte { op_index: 42, byte_in_op: 17 };
```

The operation counter spans programs *and* erases, and both honour the crash point: the partial bytes are applied, the stats are updated, and the call returns `SimFlashError::PowerLoss`, leaving the medium in exactly the torn state real hardware would. Programs are also AND-merged (`mem[i] &= buf[i]`) rather than assigned, which is what NOR flash does and what makes a splice attack — overwriting without erasing — expressible.

Then reopen the engine over the same `flash.mem` and assert the recovered state equals the acknowledged prefix. Nothing more, nothing less: no acknowledged write lost, no uncommitted write resurrected.

## What `SimFlash` models

Beyond the [`Flash`](https://github.com/ja7ad/slate/tree/main/crates/slate-kv-hal) contract, it enforces the parts real NOR flash enforces and file-backed emulation quietly forgives:

| Behaviour                | Modelled as                                                                          |
|--------------------------|--------------------------------------------------------------------------------------|
| Program-once-per-erase   | Per-page `programmed` flags; a second program returns `AlreadyProgrammed`            |
| Erased state is all-ones | `erase` fills the block with `0xFF` and clears its `programmed` flags                |
| Alignment                | Unaligned address or length returns `Unaligned`; past-capacity returns `OutOfBounds` |
| NOR write semantics      | Programs clear bits only (`&=`), never set them                                      |
| Bad blocks               | `bad_blocks: BTreeSet<u32>` — **read-only enforcement, see below**                   |
| Power loss               | `Crash::AtByte { op_index, byte_in_op }`, byte-exact, across programs and erases     |

Because `mem` is a plain `Vec<u8>`, a test can also corrupt bytes directly to simulate bit rot, or snapshot and restore an image to simulate a rollback attack. `op_log: Vec<Op>` records every read, program and erase for post-hoc assertions.

> **Two sharp edges in the fidelity, worth knowing before you trust a result.**
>
> **`bad_blocks` only fails reads.** A block inserted into `bad_blocks` returns `BadBlock` from `read`, but `program` and `erase` never consult the set and will succeed on it. Modelling a block that has worn out for *writes* needs a different mechanism than this field.
>
> **`read_lat_ms`, `prog_lat_ms` and `erase_lat_ms` are inert.** They are declared and initialised to zero, and **no code path reads them** — not `SimFlash`'s own methods, not any test. They are also whole-millisecond `u64`s, so sub-millisecond flash latencies are inexpressible even if they were wired up. Do not treat any timing derived from them as measured.

`SimCounter` completes the pair: a `MonotonicCounter` with a configurable budget, so counter exhaustion is testable without burning eFuses.

## The harness engine: `sim_db`

`sim_db::Db` is a parallel `std` wrapper over the engine, mirroring `slate-kv`'s `Db` but built over `SimFlash`/`SimCounter` instead of files. It exists because the interesting assertion is *remount* behaviour, and that needs the flash image to outlive the engine:

```rust,ignore
use slate_kv_sim::sim_db::{Db, KeySource, Options};
use slate_kv_sim::{SimCounter, SimFlash};

let opts = Options { capacity: 1024 * 1024, n_keys: 1024, ..Default::default() };
let flash = SimFlash::new(opts.capacity, 256, 4096);
let counter = SimCounter::new(100_000);

let mut db = Db::open(KeySource::Bytes([0x42; 32]), opts.clone(), flash, counter).unwrap();
db.put(b"k", b"v").unwrap();
db.commit().unwrap();                       // the acknowledged prefix ends here
db.put(b"k2", b"never committed").unwrap();

// Attribute the next writes to compaction, so WA is measured, not estimated.
db.flash_mut(|f| f.is_gc_write = true);

// Reclaim the medium, drop the engine, remount over the SAME bytes.
let (flash, counter) = db.take_flash_and_counter();
drop(db);
let db2 = Db::open(KeySource::Bytes([0x42; 32]), opts, flash, counter).unwrap();
assert_eq!(db2.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
assert_eq!(db2.get(b"k2").unwrap(), None);
```

Differences from `slate-kv`'s `Db` that will trip you up if you assume they match: `Db::open` takes the flash and counter **by value** and returns `Err(Box<(DbError, SimFlash, SimCounter)>)` on failure so a failed open does not consume the medium; `KeySource` has only the `Bytes` variant; `Options` has no `durability` field; and `Stats` is `power::Stats`, which has **no `marker_bytes` field** — see the energy caveat below.

## Test suites

| Test                   | Asserts                                                                                                                               |
|------------------------|---------------------------------------------------------------------------------------------------------------------------------------|
| `crash_monte_carlo.rs` | Randomised crash points; recovered prefix equals acknowledged prefix                                                                  |
| `gc_compaction.rs`     | `test_no_resurrection_prop3_1` — a deleted key never returns after compaction; `test_wa_accounting` — write-amplification bookkeeping |
| `rs_recovery.rs`       | A database survives injected bad blocks via RS(12,8) reconstruction                                                                   |
| `kv_demo_readpath.rs`  | The board transcript read path: put/put/commit/get, values readable across many commits, overwrite and delete resolution              |

`debug.rs` is a scratch binary for manual poking, not an assertion suite.

## Energy accounting

```rust,ignore
use slate_kv_sim::power::{report, PowerModel, Stats};

let r = report(&stats, &PowerModel::default());
assert_eq!(r.label, "ESTIMATED");
```

`report` turns flash counters into joules under a parameterised cost model. The flash *counts* are measured; the joules are not — `PowerReport` labels itself `ESTIMATED` for exactly that reason, and no power meter or board is involved.

One accounting hazard: `power::Stats` sums `user + gc + parity + ckpt` bytes and **has no `marker_bytes` field at all**, even though the engine's own `Metrics` does and counts two commit-marker pages per commit. That omission is precisely the term that scales as 1/B, so at small `b_commit` `report()` understates the total — which matters most when demonstrating the convexity that justifies batching. `src/bin/slate_energy_batch.rs` handles this by reporting two columns: what `report()` returns, and the same model constants applied to `SimFlash`'s ground-truth `bytes_programmed`.

## The `src/bin/` binaries: read the labels

These are measurement harnesses of uneven provenance. Which ones drive the real engine matters:

| Binary               | Drives the engine?                                                                                                                                      |
|----------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------|
| `crash_mc`           | **Yes.** Mounts `sim_db::Db`, injects crashes, remounts, compares against a ground-truth log. This is the crash campaign.                               |
| `energy_check`       | **Yes.** Opens the engine at varying `b_commit` and measures flash traffic.                                                                             |
| `slate_energy_batch` | **Yes.** The corrected energy/batch sweep, with the two-column reporting described above.                                                               |
| `wa_study_paper`     | **No.** A standalone GC/WA model. It imports only `rand` and `slate_kv_erasure` constants — no engine. Documents its own corrections over `wa_study`.   |
| `wa_study`           | **No.** The original GC/WA model; superseded by `wa_study_paper`, which fixes a capacity confound and a derived-rather-than-measured utilisation in it. |

So a number from `wa_study*` is a model evaluation, not an engine measurement. The engine's own write amplification is measured by `slate-kv`'s `slate_wa_buckets` example. Treat `tests/` and the engine-driving binaries as evidence; treat the model binaries as models.

## Adding a test

The pattern that works: build a `SimFlash` + `SimCounter`, mount an engine (either `sim_db::Db` or a hand-composed `Slate` — see `create_slate` in `gc_compaction.rs`), drive a workload, inject a fault, reclaim the medium with `take_flash_and_counter`, drop the engine, remount over the *same* bytes, and assert on what survived. The remount is the important part: asserting on in-RAM state proves nothing about durability.

`proptest` is already a dependency; prefer a property over a fixed scenario when the invariant is universal ("for any crash point…").

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
