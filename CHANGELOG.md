# Changelog

All notable changes to this project are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The on-flash format carries its own version (`FORMAT_VERSION`), which is
independent of the crate version and is called out explicitly whenever it moves.

## [0.6.0] — 2026-08-08

The log head is now circular: space freed by reclamation is reusable, and a
device no longer halts once the head reaches the end of the region. This closes
the gap previously described as "the most consequential item of remaining work".

**This release changes the on-flash format (`FORMAT_VERSION` 1 → 2) and is not
backward compatible.** A volume written by 0.5.x has no segment headers, so its
log cannot be ordered by allocation number; mount rejects it rather than
reconstructing an index from records replayed in the wrong order. There is no
in-place migration — reformat, or drain the old volume with 0.5.x before
upgrading.

Sustained operation and operation across restarts are both covered by tests: the
in-memory harness runs 100+ complete fill/reclaim cycles, and
`wrapped_log_survives_remount` reopens a wrapped volume from bytes on disk. The
full workspace suite passes and `clippy --workspace --all-targets -D warnings` is
clean. One design-level gap remains — segment parity is not encoded on the write
path (see *Known issues*) — so the erasure-tolerance property is verified against
the coder rather than exercised end-to-end.

### Added

- Circular log allocation. `SegTable::pick_free`/`fits_in_segment` — present but
  uncalled since they were written — are now wired, together with a head-roll
  path for the *hot* head, which previously had no relocation path at all.
- On-flash segment headers (`segment::write_header`/`read_header`), stamped when
  a segment is opened: magic, format version, allocation number, epoch, minimum
  sequence and a MAC binding the header to the volume's key hierarchy. These are
  what let a wrapped log be ordered by allocation rather than by address.
- `SegTable::rebuild_from_flash`, reconstructing segment state at mount from
  those headers. Without it the allocator would treat a populated volume as
  entirely free and hand out a segment holding live records.
- `recover::recover_spans`, replaying a list of address ranges in allocation
  order. `recover::recover` remains as a single-span wrapper.
- `epoch::checkpoint_only_async` and `Slate::checkpoint_for_reclaim_async`:
  publish a checkpoint at the current epoch *without* advancing the epoch or the
  hardware monotonic counter.
- Futile-reclaim guard (`GC_FUTILE_RELOCATION_PCT`, 97%). When a completed
  compaction relocates nearly everything it scanned, the engine reports
  `FlashFull` instead of copying live data it has nowhere to put.
- `Metrics::padding_bytes` and `CommitBytes::payload_bytes`, closing the
  accounting identity `user + gc + parity + marker + ckpt + padding = bytes
  programmed`.
- `Durability::None` (**tests and benchmarks only**) for workloads whose subject
  is allocation rather than durability.
- `SEG_DATA_BYTES`, the writable area of a segment, distinct from the `SEG_BYTES`
  allocation stride.
- Acceptance tests: `slate-kv-sim/tests/log_wrap.rs` (sustained operation across
  100+ region passes), plus `space_reuse_after_reclaim` un-ignored in
  `slate-kv/tests/esp32_defects.rs`.

### Changed

- **Format (breaking):** the log is confined to the 8 data blocks of each
  segment; the 4 parity blocks are no longer written through. Usable capacity per
  segment is 32,768 B rather than 49,152 B.
- `reserve_headroom` is sized from the largest record the instance has actually
  appended, not from `MAX_KEY_LEN + MAX_VAL_LEN`. At `b_commit = 8` the
  worst-case assumption reserved 11,520 B of a 32,768 B data area, so a third of
  every segment was erased and never used. Measured on ESP32-C3: steady-state
  erases fell from 115.8 to 84.7 per 1,000 records (−27%), a 1.37× extension of
  erase-limited lifetime.
- Reclamation no longer advances the epoch. It previously called
  `seal_epoch_now_async` to qualify a victim, consuming a hardware counter
  increment per reclaim; the device trace showed the epoch jumping by two across
  a single interval. It now advances by exactly one per Θ trigger.
- Checkpoint selection at mount orders by `(epoch, seq)` rather than by epoch
  alone. Two valid checkpoints can now share an epoch, and an epoch-only
  comparison let a stale one win on slot order.
- Both checkpoint paths program pending batches before serialising the index.
- Reported write amplification now includes page padding. On the ESP32-C3 trace
  this moves the figure from 2.7435 to 3.0408 — the same physical writes, counted
  completely. **Amplification figures from 0.5.x understate the true cost by
  ~9.8% and are not comparable to 0.6.0 figures.**
- `slate-kv-sim`'s `power::Stats` gained `marker_bytes`, `segments` and
  `gc_open_failed`. `power::report` previously summed energy over every bucket
  except commit markers — the largest overhead term at small batch sizes — so
  **every simulator energy figure produced before this release understates write
  energy.**
- `HeadState` derives `Default` and tracks `max_record_bytes`.

### Removed

- `Slate::relocate_cold_head_async`, superseded by `roll_head_async`. It moved
  the cold head without writing a segment header, which a wrapped log now needs
  in order to be ordered by allocation number. It was private, so this is not a
  breaking API change.

### Fixed

- Device halt at flash exhaustion with most of the region free and erased. On
  ESP32-C3 the demo previously stopped at 8,112 records; it now runs past 20,900
  with the head cycling through reclaimed segments.
- Compaction scanning from the segment base, where a header now sits, and
  aborting at the first byte.
- **Data loss across a remount after the log wrapped.** The reclaim watermark
  was set one segment too high: `checkpoint_only_async` and `seal_epoch_now_async`
  both assigned `ckpt_seg_seq = current_seg_seq() + 1`, while `pick_victim`
  reclaims any segment with `seg_seq < ckpt_seg_seq`. The `+ 1` therefore made
  the checkpoint's *own* segment eligible — and that segment is exactly where the
  checkpoint's `replay_from` points. Reclaim erased it, so a later mount anchored
  replay in erased flash, rejected the first commit marker it met (the chain
  covering the records in between was gone), applied zero records, and returned
  `None` for every key the checkpoint had just restored correctly. The watermark
  is now `current_seg_seq()`, protecting the checkpoint's own segment. Covered by
  `esp32_defects::wrapped_log_survives_remount`.
- Tail loss on reopen after an epoch roll. The batch-flush helper introduced for
  the counter-preserving checkpoint stamped its commit marker with `acked_seq` —
  the *previous* commit's high-water mark — rather than `next_seq - 1`, so
  recovery saw a marker that did not cover the batch beneath it and dropped the
  tail. It also never advanced `acked_seq`, and skipped the segment-boundary roll
  that `commit_inner_async` performs, allowing a checkpoint to record a head that
  had already overrun its segment.
- Genesis segment skipped during replay. At first mount the heads are placed
  directly at `data_base` without opening a segment, so segment 0 carries records
  and no header. The mount-time replay anchor rejected any segment the table
  called `Free`, which includes that one, discarding every record written before
  the first roll.
- Unbounded relocation near capacity. With a live set larger than the log area
  the engine accepted every write while relocating indefinitely: measured at
  321,871 relocations to place 8,000 keys, 67,887 erases and a write
  amplification of 229 — roughly 6,789 erase cycles per block in a single test
  run — while reporting success. It now reports `FlashFull`.

### Known issues

- Segment parity is still not encoded on the data path. `segment::encode_parity`
  has no production caller, so the RS(12,8) tolerance verified in simulation is
  not exercised by the shipped write path. The 0.6.0 format reserves the parity
  blocks, so closing this will not require another format break.
- RAM, mount-replay yielding, and the sequential-key fingerprint collision rate
  are unchanged from 0.5.0; see the README's *Known limitations*.

## [0.5.0] and earlier

Not recorded here. This file starts at 0.6.0; earlier history is in the commit
log and in `docs/specification.md`.

[0.6.0]: https://github.com/ja7ad/slate/releases/tag/v0.6.0
