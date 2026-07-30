# SLATE Implementation Specification

**Document:** implementation specification and conformance report for the SLATE
embedded key–value engine.
**Implementation:** <https://github.com/ja7ad/slate> (Rust workspace, version 0.4.0).
**Revision described:** git `970324f` (`970324f20f3fc7d7df4249c4d51122f9a1a5e61c`, 2026-07-29),
plus the working-tree measurement harnesses named in
[Section 6](#6-conformance-and-measured-behaviour).

This document is the implementation-side companion to the formal treatment of SLATE.
The formal paper carries the threat model, the theorems and their proofs; this
specification carries the on-flash byte layout, the operational semantics, the Rust
API surface, the configuration arithmetic, the measurement harnesses that establish
conformance, and the complete log of known deviations between what the project
documents claim and what the code at this revision does.

Every quantitative statement below is traceable to a file under
`docs/proposal/data/` and to a `cargo` command that regenerates it. Where a data
file and a prior prose description disagree, this document follows the data file and
says so in [Section 7.6](#76-disagreements-between-data-files-and-earlier-prose).

---

## Table of contents

1. [Scope and normative language](#1-scope-and-normative-language)
2. [On-flash format](#2-on-flash-format)
3. [Operational semantics](#3-operational-semantics)
4. [Configuration constants and derived sizes](#4-configuration-constants-and-derived-sizes)
5. [Rust API surface](#5-rust-api-surface)
6. [Conformance and measured behaviour](#6-conformance-and-measured-behaviour)
7. [Known deviations from this specification](#7-known-deviations-from-this-specification)
8. [Remaining work](#8-remaining-work)
9. [Appendix A: data file inventory](#9-appendix-a-data-file-inventory)

---

## 1. Scope and normative language

### 1.1 Scope

SLATE is a log-structured, authenticated key–value engine for
microcontroller-class raw NOR flash: no filesystem beneath it, no
battery-backed write cache, no flash translation layer, and typically under
100 KiB of RAM in which the whole engine — index, batch buffers, checkpoint
buffer and working state — must reside simultaneously.

This specification covers:

- the byte-level on-flash format (records, commit markers, XOR parity pages,
  checkpoints, segment headers, region layout);
- the operational semantics of `put`, `get`, `delete`, `commit`, epoch sealing,
  checkpointing, garbage collection and mount-time recovery;
- the configuration constants and the arithmetic that derives every dependent
  size, including the RAM working set;
- the Rust trait and type surface an integrator must implement or call;
- the conformance evidence: what has been measured, on which platform, by which
  command;
- the deviations: where the implementation does not meet this specification, and
  where project documentation asserts a mechanism the source does not contain.

It does not cover: wire protocols (there are none), multi-writer or
multi-process access (out of scope by construction, see
[Section 1.3](#13-system-model)), or wear-levelling policy beyond the segment
reclamation described in [Section 3.8](#38-garbage-collection).

### 1.2 Normative language

The key words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT** and **MAY** are
used as follows.

- **MUST** / **MUST NOT** — an absolute requirement. A violation is a
  correctness bug: the durability, freshness or erasure-tolerance properties of
  [Section 3](#3-operational-semantics) do not hold if it is violated. This
  applies both to the engine and to the flash and counter drivers an integrator
  supplies.
- **SHOULD** / **SHOULD NOT** — a strong recommendation. A deployment may
  deviate with a stated reason; the consequence is stated at each occurrence.
- **MAY** — genuinely optional.

Requirements addressed to the *driver* (the integrator's `Flash`,
`AsyncFlash` and `MonotonicCounter` implementations) are marked explicitly,
because the engine cannot check them and its guarantees are conditional on them.

### 1.3 System model

**Single writer.** Exactly one `Db` (or `Slate`) instance MUST have the flash
region open for writing at any time. The engine holds the log head, the RAM
index and the batch buffers as owned state and performs no locking against
other processes. Two instances opened on the same region will each program
pages the other believes are erased, and the resulting volume is unmountable.
The std wrapper (`slate-kv`) enforces single-instance access within a process
via interior mutability behind `&self`; it does not and cannot enforce it
across processes.

**No dynamic allocation in the core.** The four lower crates
(`slate-kv-core`, `slate-kv-crypto`, `slate-kv-erasure`, `slate-kv-hal`) are
`#![no_std]` and carry `#![forbid(unsafe_code)]`. Every buffer is either an
owned array inside the engine struct or a slice borrowed from the caller.
Consequently every RAM cost is a compile-time constant, and
[Section 4.4](#44-ram-working-set) computes it exactly.

**Erase-before-write NOR semantics.** The engine assumes program-once-per-erase
flash with detectable tearing: a page MUST be erased before it is programmed,
`program` completion means durable, and a torn program is detectable (the
erased byte is `0xFF`, and a partially programmed page fails either its magic
byte, its length fields, or its authentication tag).

**At-rest adversary.** The rollback-resistance property assumes an adversary
who can read and write the flash image while the device is *powered off*, and
who cannot roll back the external monotonic counter. An adversary who can
write flash while the device runs, or who can rewind the counter, is out of
scope. This is stated as a limitation, not a completeness claim.

### 1.4 Evidence conventions

Three distinctions are marked at every table in
[Section 6](#6-conformance-and-measured-behaviour), because conflating them is
the commonest way an embedded storage claim becomes untrue.

| Label | Meaning |
|---|---|
| **Measured (host)** | The real engine ran against `FileFlash`, a file-backed NOR emulation on the development host's APFS filesystem. Byte counts and page counts are exact; wall-clock times characterise the host, not a device. |
| **Measured (simulated)** | The real engine ran against `SimFlash`, an in-RAM NOR simulator, or against a purpose-built latency adapter. Used where a physical experiment is impractical: exhaustive erasure enumeration, deterministic crash injection, yield-span accounting. |
| **Measured (device)** | Real ESP32-C3 silicon, real NOR flash, counters reported by the firmware over the serial link. |
| **Modelled** | Output of a closed form or of a model that is *not* the SLATE engine (the garbage-collection study, the energy model). Useful for trends; not evidence about hardware. |

No number crosses between platforms. Absolute latencies measured on the host
are host properties: see [Section 6.9](#69-throughput-and-latency-host).

### 1.5 Crate layout

| Crate | `no_std` | `forbid(unsafe_code)` | Source lines | Test lines | Role |
|---|---|---|---|---|---|
| `slate-kv-core` | yes | yes | 3,931 | 142 | Format codec, log, index, epoch/checkpoint, GC, recovery, async engine |
| `slate-kv-crypto` | yes | yes | 467 | 0 | HKDF key schedule, ChaCha20-Poly1305 record/checkpoint sealing, HMAC-SHA256 markers |
| `slate-kv-erasure` | yes | yes | 372 | 0 | $\mathrm{GF}(2^8)$ arithmetic, Cauchy matrices, RS(12,8) encode/reconstruct |
| `slate-kv-hal` | yes | yes | 473 | 0 | `Flash`, `AsyncFlash`, `MonotonicCounter`, `AsyncMonotonicCounter`, blocking bridges |
| `slate-kv` | no | no | 1,201 | 957 | std wrapper: `Db`, `Options`, `FileFlash`, `FileCounter`, metrics plumbing |
| `slate-kv-sim` | no | no | 1,747 | 690 | `SimFlash` simulator, fault-injection and measurement harnesses |
| `slate-kv-cli` | no | no | 146 | 0 | `get`/`put`/`del`/`stats` command-line tool |
| `slate-kv-ffi` | no | no | 363 | 0 | C ABI |

Engine source total 6,444 lines; workspace source total 8,700 lines; test
source total 1,789 lines. The ESP32 target crate is a further 1,477 lines and
the Go binding 719 lines. Source: `docs/proposal/data/provenance.json`.

The four lower crates are the engine proper. An integrator targeting bare metal
depends on `slate-kv-core` plus `slate-kv-hal` and supplies the two driver
traits; `slate-kv` exists to run the identical engine on a host filesystem so
that the measurements of [Section 6](#6-conformance-and-measured-behaviour) can
be made at all.

---

## 2. On-flash format

### 2.1 Requirements on the flash driver

A conforming `Flash` or `AsyncFlash` implementation:

- **MUST** report a `page_size()` that is the true program granularity, a
  `block_size()` that is an integer multiple of it, and a `capacity()` in bytes.
- **MUST** reject (or fail) a `program` whose target page has been programmed
  since its last `erase`. The engine relies on this to detect a torn tail; a
  driver that silently permits re-programming converts a detectable tear into
  undetectable corruption.
- **MUST** treat `program` completion as durability. A driver that returns
  before the bytes are on stable media breaks prefix durability, and the engine
  cannot detect this.
- **MUST** return `0xFF` for every byte of an erased region.
- **MUST** report a failed erase as an error, and **SHOULD** surface which
  block failed. The RS(12,8) reconstruction of
  [Section 3.8](#38-garbage-collection) recovers *declared* erasures; a block
  that returns wrong data without announcing itself is not recoverable by the
  code (see [Section 6.5](#65-erasure-coding-pure-computation)).
- **MUST NOT** exceed `capacity() >` $2^{24}$ bytes = 16 MiB. `mount` rejects a
  larger region, because the index stores 24-bit offsets
  (`OFF_BITS = 24`).

`erase` is a single indivisible operation. In the async trait an
implementation MAY await internally (DMA completion, status-poll interrupt),
but a caller MUST NOT be able to observe a half-erased block, and cancelling
the returned future MUST NOT abort an erase already latched into the die.

### 2.2 Region layout

The flash region is partitioned as follows, from offset 0 upward.

| Region | Extent | Contents |
|---|---|---|
| Reserved | blocks $0 \dots$ `CKPT_BASE_BLOCK` $-1$ | Not used by the engine (`CKPT_BASE_BLOCK = 2`) |
| Checkpoint slot 0 | `ckpt_slot_addr(0)`, length `MAX_CKPT_LEN` | Serialized index snapshot, AEAD-sealed |
| Checkpoint slot 1 | `ckpt_slot_addr(1)`, length `MAX_CKPT_LEN` | Previous snapshot (`CKPT_SLOTS = 2`) |
| Append log | `data_base_offset()` $\dots$ `capacity()` | Records, XOR parity pages, commit markers; divided into segments |

The slot addresses and the log base are computed from the runtime block size,
so one binary is correct for 256 B, 4 KiB and 64 KiB block geometries:

$$\text{ckpt\_blocks\_per\_slot}(B_{blk}) = \left\lceil \frac{\texttt{MAX\_CKPT\_LEN}}{B_{blk}} \right\rceil$$

$$\text{ckpt\_slot\_addr}(s, B_{blk}) = \bigl(\texttt{CKPT\_BASE\_BLOCK} + s \cdot \text{ckpt\_blocks\_per\_slot}(B_{blk})\bigr) \cdot B_{blk}$$

$$\text{data\_base\_offset}(B_{blk}) = \bigl(\texttt{CKPT\_BASE\_BLOCK} + \texttt{CKPT\_SLOTS} \cdot \text{ckpt\_blocks\_per\_slot}(B_{blk})\bigr) \cdot B_{blk}$$

The ceiling is normative, not cosmetic. A slot needing 64.03 blocks needs 65;
truncating to 64 places slot 1 one block inside slot 0, so programming the new
checkpoint erases the block holding the tail of the old one, and a crash inside
that window leaves *neither* slot readable. Every site that addresses a slot
**MUST** call the helper rather than recomputing the division.

At the shipped ESP32-C3 geometry ($B_{blk} = 4096$, `MAX_CKPT_LEN` $= 262{,}276$):

$$\text{ckpt\_blocks\_per\_slot} = \left\lceil \frac{262276}{4096} \right\rceil = 65, \qquad \text{data\_base\_offset} = (2 + 2 \cdot 65) \cdot 4096 = 540{,}672$$

which matches the `data_base = 540672` reported by the device run
(`docs/proposal/data/device_c3_analysis.json`).

### 2.3 Segment geometry

The log region is divided into fixed-size segments. A segment is 12 erase
blocks: 8 data blocks and 4 parity blocks, giving RS(12,8).

$$\texttt{SEG\_BYTES} = (\texttt{RS\_K} + \texttt{RS\_M}) \cdot B_{blk} = 12 \cdot 4096 = 49{,}152 \text{ B}$$

The number of whole segments in a region, capped at `MAX_SEGMENTS = 128`:

$$N_{seg} = \min\left(\left\lfloor \frac{\texttt{capacity} - \text{data\_base\_offset}}{\texttt{SEG\_BYTES}} \right\rfloor,\ 128\right)$$

For the 2 MiB ESP32 region: $\lfloor (2097152 - 540672)/49152 \rfloor = 31$
segments, ending at byte $540672 + 31 \cdot 49152 = 2{,}064{,}384$. Both figures
are confirmed by measurement: `wa_buckets.csv` reports `segments = 31` and
`seg_end = 2064384` on every row.

A segment table entry tracks the segment's identifier, its live byte count, the
lowest record sequence number it contains, its allocation number `seg_seq`, and
its state: `OpenHot`, `OpenCold`, `Sealed` or `Free`.

Parity is written once, at seal time, over the 8 data blocks. The space
overhead is therefore exactly

$$\frac{\texttt{RS\_M}}{\texttt{RS\_K}} = \frac{4}{8} = 0.5 \text{ parity bytes per data byte}, \qquad \frac{\texttt{RS\_N}}{\texttt{RS\_K}} = 1.5 \text{ stripe bytes per data byte}$$

a multiplicative floor of $1.5\times$ on write amplification that no
configuration can avoid. `erasure.csv` confirms
`parity_blocks/data_blocks = 0.500000` and
`stripe_bytes/data_bytes = 1.500000` from the encoder itself.

### 2.4 Record encoding

A record on flash is a 28-byte plaintext header, followed by the
ChaCha20-Poly1305 ciphertext of the concatenated key and value, followed by
the 16-byte Poly1305 tag. The header is the AEAD associated data, so every
header field is authenticated even though it is not encrypted.

Header (`REC_HDR_LEN = 28`), little-endian throughout:

| Offset | Length | Field | Type | Notes |
|---|---|---|---|---|
| 0 | 1 | `magic` | `u8` | `MAGIC_REC = 0x5A`; a decode with any other value is `FormatError` |
| 1 | 8 | `seq` | `u64` | Strictly increasing, never reset |
| 9 | 1 | `op` | `u8` | `OP_PUT = 0x00`, `OP_DEL = 0x01`; any other value is `FormatError` |
| 10 | 2 | `fp` | `u16` | Key fingerprint, for replay-time collision resolution |
| 12 | 2 | `klen` | `u16` | Key length; `FormatError` if `> MAX_KEY_LEN` |
| 14 | 2 | `vlen` | `u16` | Value length; `FormatError` if `> MAX_VAL_LEN` |
| 16 | 12 | `nonce` | `[u8; 12]` | AEAD nonce, see [Section 2.5](#25-nonce-and-key-schedule) |

Total framed length of one record:

$$L_{rec} = \texttt{REC\_OVERHEAD} + \texttt{klen} + \texttt{vlen} = (28 + 16) + \texttt{klen} + \texttt{vlen} = 44 + \texttt{klen} + \texttt{vlen}$$

Worked example, the shipped ESP32 demo workload: a 12-byte key and an 18-byte
value give $44 + 30 = 74$ B, which is exactly the `record_bytes = 74` and
`key_plus_value_bytes = 30` reported by the device run.

Records are packed contiguously into the batch buffer and programmed as whole
pages at commit time. A record therefore **MAY** straddle a page boundary, and
at this revision **MAY** also straddle a segment boundary — which is one of
the two format-level causes of the space-reuse gap of
[Section 7.5](#75-standing-gaps).

### 2.5 Nonce and key schedule

The device key $K$ is 32 bytes and is never written to flash, never `Debug`- or
`Display`-formatted, and is zeroized on drop. All working keys are HKDF-SHA256
derivations of it:

$$\mathrm{prk} = \text{HKDF-Extract}(\text{salt} = \texttt{"SLATE/v1"},\ \mathrm{ikm} = K)$$

$$k_{cm} = \text{Expand}(\mathrm{prk}, \texttt{"cm"}), \quad k_{ckpt} = \text{Expand}(\mathrm{prk}, \texttt{"ckpt"}), \quad k_{ctr} = \text{Expand}(\mathrm{prk}, \texttt{"ctr"})$$

$$k_{rec}^{(e)} = \text{Expand}\bigl(\mathrm{prk},\ \texttt{"rec"} \parallel \mathrm{le64}(e)\bigr)$$

The record subkey is rolled once per epoch. Because the derivation is
deterministic, an older epoch's record key is always re-derivable from the
device key without ever storing it — which is what lets recovery and garbage
collection read records sealed under an epoch that is no longer open.

The 96-bit record nonce is:

| Bytes | Field |
|---|---|
| 0..8 | `seq` as `u64` little-endian |
| 8..12 | epoch discriminator as `u32` little-endian |

The sequence number alone guarantees uniqueness, since it is a strictly
increasing total order that is never reset. The epoch discriminator is carried
so that a reader knows which epoch subkey to derive. Because the header is the
associated data, a flipped epoch byte surfaces as `Tampered` rather than as
silently wrong plaintext. `seal_epoch` **MUST** refuse to roll past
`MAX_REC_EPOCH` $= 2^{32}-1$ rather than alias epoch keys.

### 2.6 Commit marker

A commit marker is 83 bytes (`CM_LEN = 83`), authenticated with HMAC-SHA256
under $k_{cm}$ over its own first 51 bytes.

| Offset | Length | Field | Type | Notes |
|---|---|---|---|---|
| 0 | 1 | `magic` | `u8` | `MAGIC_CM = 0x5C` |
| 1 | 8 | `seq_max` | `u64` | Highest sequence number this marker acknowledges |
| 9 | 8 | `epoch` | `u64` | Epoch the batch was sealed under |
| 17 | 2 | `xor_pages` | `u16` | Data pages covered by the preceding XOR page |
| 19 | 32 | `chi` | `[u8; 32]` | Chain value $\chi$ after folding every record in the batch |
| 51 | 32 | `tau_cm` | `[u8; 32]` | HMAC-SHA256 over bytes 0..51 |

Two copies of the marker are programmed on consecutive pages. Verification
tries copy 1 and falls back to copy 2. Note the limit of that redundancy,
measured in [Section 6.6](#66-at-rest-tampering-host-and-simulated): the
recovery scanner dispatches on the *first byte* at an offset, so a marker whose
magic byte has been destroyed is not reached by the fallback path at all. The
twin protects against a tear inside the marker body, not against loss of its
leading byte.

Because a marker occupies at least one whole page regardless of its 83-byte
logical size, and two copies are written, the marker cost per commit is

$$M = 2 \cdot \left\lceil \frac{\texttt{CM\_LEN}}{P} \right\rceil \cdot P = 2P \text{ bytes for } P \ge 83$$

which is 512 B at $P = 256$. The measured marker bytes per commit in
`wa_buckets.csv` range from 486 to 509 B across the sweep, the shortfall being
the trailing partial batch that is never flushed. This fixed cost, divided by
the batch size, is the dominant term in write amplification at small batches
([Section 6.7](#67-write-amplification-host)).

### 2.7 XOR head page

Before the commit marker, `Log::commit_async` programs one XOR parity page
covering the data pages of the batch: byte $j$ of the parity page is the XOR of
byte $j$ of each covered data page, with byte 0 overwritten by
`MAGIC_XOR = 0x58`. The count of covered pages travels in the marker's
`xor_pages` field, so recovery knows the extent of the protected span. This is
an intra-batch tear-protection device and is independent of the segment-level
RS(12,8) parity of [Section 2.3](#23-segment-geometry).

### 2.8 Checkpoint encoding

A checkpoint is a 76-byte plaintext header (`CKPT_HDR_LEN = 76`) acting as AEAD
associated data, followed by the ChaCha20-Poly1305 ciphertext of the serialized
index, followed by the 16-byte tag.

| Offset | Length | Field | Type | Notes |
|---|---|---|---|---|
| 0 | 1 | `magic` | `u8` | `MAGIC_CKPT = 0xCF` |
| 1 | 1 | `format_version` | `u8` | |
| 2 | 8 | `epoch` | `u64` | Epoch this checkpoint closes |
| 10 | 8 | `seq` | `u64` | Next sequence number at seal time |
| 18 | 8 | `seg_seq` | `u64` | Segment allocation number at seal time |
| 26 | 4 | `write_offset` | `u32` | Log head at seal time; tail replay starts here |
| 30 | 2 | `n_keys` | `u16` | Live keys in the snapshot |
| 32 | 4 | `ct_len` | `u32` | Ciphertext length; `FormatError` if `> MAX_CKPT_LEN` |
| 36 | 32 | `chi` | `[u8; 32]` | Chain value at seal time, the anchor $D_{ckpt}$ |
| 68 | 8 | `mc` | `u64` | Monotonic counter value observed at seal time |

The serialized index is 4 bytes per slot plus a 5-byte entry per stash slot:

$$\text{index\_serialized\_len}(n) = 4n + 5 \cdot \texttt{STASH\_SIZE} = 4n + 40$$

$$\text{ckpt\_len\_for\_slots}(n) = \texttt{CKPT\_HDR\_LEN} + \text{index\_serialized\_len}(n) + 16 = 4n + 132$$

$$\texttt{MAX\_CKPT\_LEN} = \text{ckpt\_len\_for\_slots}(\texttt{MAX\_INDEX\_SLOTS}) = 4 \cdot 65536 + 132 = 262{,}276 \text{ B}$$

For the shipped 8,192-slot index this is
$\text{ckpt\_len\_for\_slots}(8192) = 32{,}900$ B — the single largest term in
the RAM budget, and the reason the budget is exceeded
([Section 4.4](#44-ram-working-set)). The device's own checkpoint writes are
33,024 B, which is 9 erase blocks at 4 KiB: the format rounds the 32,900 B
payload up to whole pages and blocks.

A compile-time assertion enforces
$\text{ckpt\_len\_for\_slots}(\texttt{MAX\_INDEX\_SLOTS}) \le \texttt{MAX\_CKPT\_LEN}$,
so index capacity and checkpoint capacity cannot drift apart.

### 2.9 Segment header

The format defines a 59-byte segment header. **At this revision no code path
ever writes one**, and the recovery scanner treats the log as one flat append
region. The structure is specified here because closing the space-reuse gap of
[Section 7.5](#75-standing-gaps) requires writing it, and because
`recover::scan_segment_headers` — the ordering mechanism a circular log needs
— already exists to read it.

| Offset | Length | Field | Type | Notes |
|---|---|---|---|---|
| 0 | 1 | `magic` | `u8` | `MAGIC_SEG = 0x51` |
| 1 | 1 | `format_version` | `u8` | |
| 2 | 8 | `seg_seq` | `u64` | Segment allocation number; the circular-log ordering key |
| 10 | 8 | `epoch` | `u64` | Epoch at segment open |
| 18 | 8 | `minseq` | `u64` | Lowest record sequence number in the segment |
| 26 | 1 | `sealed` | `u8` | `0xFF` open, `0x00` sealed |
| 27 | 32 | `hdr_mac` | `[u8; 32]` | HMAC over bytes 0..27 |

### 2.10 Magic byte registry

| Value | Constant | Structure |
|---|---|---|
| `0x5A` | `MAGIC_REC` | Record header |
| `0x5C` | `MAGIC_CM` | Commit marker |
| `0x51` | `MAGIC_SEG` | Segment header (never written at this revision) |
| `0x58` | `MAGIC_XOR` | XOR parity head page |
| `0xCF` | `MAGIC_CKPT` | Checkpoint header |
| `0xFF` | `ERASED_BYTE` | Erased flash; terminates the recovery scan |

The values are mutually distinct and distinct from `0xFF`, which is what lets
the single-byte dispatch in the recovery scanner work.

---

## 3. Operational semantics

### 3.1 Put

`put(key, value)` appends the record to the in-RAM hot batch buffer and
returns. Concretely, `Slate::append_hot` frames the record
([Section 2.4](#24-record-encoding)), seals it under $k_{rec}^{(e)}$, folds it
into the chain, allocates $L_{rec}$ bytes in the batch buffer, updates the RAM
index to point at the offset the record *will* occupy, and increments
`next_seq`.

`append_hot` touches no flash. It is therefore the one engine operation that is
correctly synchronous even in the async build. It returns `BatchFull` when the
batch buffer cannot hold another record, at which point the caller **MUST**
commit before retrying.

Note the ordering: the index is updated at append time, before the record is
durable. This is safe because the index is RAM-only and is reconstructed at
mount from the checkpoint plus the replayed tail
([Section 3.7](#37-tail-replay)); an uncommitted index entry cannot survive a
power loss.

`put` **MUST NOT** be treated as durable. The acknowledgement rule is in
[Section 3.2](#32-commit-and-the-acknowledgement-rule).

### 3.2 Commit and the acknowledgement rule

`commit` is the durability boundary. One commit performs, in order:

1. pad the batch buffer to a page multiple and program the data pages;
2. program the XOR parity page over those data pages
   ([Section 2.7](#27-xor-head-page));
3. build the commit marker over `seq_max`, `epoch`, `xor_pages` and the current
   chain value $\chi$, and program **two** copies on consecutive pages;
4. advance `acked_seq` to `seq_max`;
5. clear the batch buffer.

**The acknowledgement rule.** An operation is acknowledged if and only if a
commit marker whose `seq_max` is greater than or equal to the operation's `seq`
has been programmed and verified. No other event — not `put` returning, not
the data page landing — constitutes acknowledgement.

**Theorem (prefix durability).** If power is lost at an arbitrary byte offset of
an arbitrary flash operation, then after `mount` the recovered state is a prefix
of the acknowledged operation sequence: every operation whose `seq` is at most
the recovered `acked_seq` is present, and no operation beyond it is visible.

The mechanism is that recovery accepts a batch only when three independent
conditions hold simultaneously ([Section 3.7](#37-tail-replay)): the marker
verifies under $k_{cm}$, its `seq_max` equals the last sequence number in the
pending batch, and its `chi` equals the chain value recomputed over the records
actually found on flash. A torn data page fails the third; a forged or corrupted
marker fails the first; a marker spliced from a different batch fails the
second. The conformance evidence is 20,000 crash-injection trials with zero
violations ([Section 6.3](#63-crash-injection-simulated)).

There is deliberately **no yield point inside commit**. The two-marker window is
a durability property: a task that suspended between copy 1 and copy 2 and was
cancelled would leave a window the format does not contemplate. This is a
normative constraint on any future async work: an implementation **MUST NOT**
introduce an await point between the two marker programs.

The commit batch size $B$ (`b_commit`) is the engine's cost–durability dial.
At $B = 1$ every record pays a full marker cost; at $B = 128$ the marker is
amortised across 128 records. An application that raises $B$ accepts losing up
to $B - 1$ records on power failure in exchange for the amplification and
throughput improvements of [Sections 6.7](#67-write-amplification-host) and
[6.9](#69-throughput-and-latency-host). `put_durable` is `put` followed by an
immediate `commit`, i.e. $B = 1$ semantics for a single call.

### 3.3 Get

`get(key)` computes the key's 64-bit FNV-1a hash, derives the two candidate
buckets and the fingerprint, and collects every candidate offset from both
buckets and the whole stash. For each candidate it reads the record header from
flash, AEAD-opens the record, and compares the full key. The first match whose
key compares equal is returned; a tombstone (`OP_DEL`) returns `None`.

Candidate collection scans exactly

$$\texttt{probes} = 2 \cdot \texttt{BUCKET\_SLOTS} + \texttt{STASH\_SIZE} = 2 \cdot 4 + 8 = 16 \text{ slots}$$

unconditionally, with no early exit. Lookup cost in the index is therefore a
constant independent of load factor — mean equals maximum equals worst case,
which `index_ram.csv` confirms on all 112 rows
([Section 6.11](#611-index-behaviour-in-ram)). The variable cost is the number
of *flash reads* the candidate set provokes, which is governed by the
fingerprint collision rate.

Reads do not touch the batch buffer: `get` resolves through the index to a flash
offset, so a key that has been `put` but not yet committed is visible only
because its index entry already points at the offset the record will occupy.
A `get` on an uncommitted key therefore **MUST NOT** be relied upon across a
power loss.

### 3.4 Delete

`delete(key)` appends a tombstone record (`op = OP_DEL`, `vlen = 0`) and removes
the key from the RAM index. The tombstone is a normal record: it is sealed,
chained, counted in `user_bytes`, and acknowledged by the next commit under the
same rule as a put. Space held by the superseded record is reclaimed only by
garbage collection ([Section 3.8](#38-garbage-collection)).

`delete_durable` is `delete` followed by `commit`.

### 3.5 Epoch sealing and checkpointing

An epoch seal is the operation that makes mount cheap and makes rollback
detectable. It performs:

1. serialize the RAM index into the checkpoint buffer
   ([Section 2.8](#28-checkpoint-encoding));
2. fill the header with the current `epoch`, `seq`, `seg_seq`, `write_offset`,
   `n_keys`, chain value $\chi$ and the counter value `mc`;
3. AEAD-seal the payload under $k_{ckpt}$ with the header as associated data;
4. erase and program the *inactive* checkpoint slot (`next_ckpt_slot()`),
   leaving the active one intact;
5. increment the external monotonic counter;
6. roll the record subkey to the new epoch and re-anchor the chain.

The chain re-anchors deterministically from the checkpoint digest, so a
recovered chain is bound to the checkpoint it came from:

$$\chi_0^{(e)} = H\bigl(\texttt{"slate/epoch"} \parallel \mathrm{le64}(e) \parallel D_{ckpt}(e)\bigr), \qquad \chi \leftarrow H(\chi \parallel r)$$

with $H = $ SHA-256 and $r$ the framed record bytes. The fold is $O(1)$ per
record.

A seal is triggered automatically after `THETA` $= 16{,}384$ records in an
epoch, or explicitly by `seal_epoch`. The counter budget is validated against
the expected device lifetime at configuration time:

$$\text{required counter increments} = \left\lfloor \frac{\texttt{expected\_life\_ops}}{\texttt{THETA}} \right\rfloor$$

and `SlateConfig::validate` returns `CounterBudgetExceeded` if the configured
`counter_budget` is smaller. It also requires
`arena_bytes` $\ge 2 \cdot$ `expected_live_bytes` (`CapacityTooSmall`
otherwise), which is the engine's way of refusing a configuration in which
garbage collection cannot make progress.

Ordering is normative: the new checkpoint **MUST** be durable before the counter
is incremented. This is what makes the crash window recoverable — see the
boot rule below, where a checkpoint one epoch ahead of the counter is the
signature of a crash inside exactly this window.

### 3.6 Mount and the boot rule

`mount` is an $O(1)$ freshness check followed by an $O(\Theta)$ tail replay. In
order:

1. Reject the volume if `capacity()` $> 2^{24}$ (`FormatError`).
2. Read **every** populated checkpoint slot, AEAD-verify it, and select the one
   with the highest epoch. Both slots are read, so the number of populated slots
   is a real (but bounded) term in mount cost — `CKPT_SLOTS` $= 2$.
3. Read the external monotonic counter into $\mathrm{MC}^*$; let $m$ be the
   selected checkpoint's epoch. Apply the boot rule:
   - if $m < \mathrm{MC}^*$, refuse with `Rollback` — the image is stale;
   - if $m > \mathrm{MC}^* + 1$, refuse with `Tampered` — unreachable without
     forgery;
   - if $m = \mathrm{MC}^* + 1$, the device crashed inside the seal window:
     re-run the counter increment now, then accept;
   - if $m = \mathrm{MC}^*$, accept.
4. Re-anchor the chain from the checkpoint digest, set `epoch` $= m + 1$,
   `next_seq` from the checkpoint, and `acked_seq` $=$ `next_seq` $- 1$.
5. Record the security mode from the counter kind
   ([Section 3.10](#310-security-modes)).
6. Replay the tail from the checkpoint's `write_offset`
   ([Section 3.7](#37-tail-replay)).

If the counter reports `CounterKind::None`, step 3 is skipped entirely and the
security mode is `NoRollbackProtection`. An integrator **MUST NOT** deploy a
`None` counter and describe the result as rollback-resistant.

The measured behaviour of this rule under adversarial input is in
[Sections 6.4](#64-rollback-resistance-simulated) and
[6.6](#66-at-rest-tampering-host-and-simulated): 5,000 of 5,000 splice attacks
refused, and both halves of the rule (stale epoch, epoch gap) exercised
separately.

### 3.7 Tail replay

Replay walks forward from the checkpoint's `write_offset` one byte at a time,
dispatching on the first byte at each position.

| First byte | Action |
|---|---|
| `0xFF` (`ERASED_BYTE`) | If mid-page, skip to the next page boundary and continue; if page-aligned, the log ends — stop |
| `0x5C` (`MAGIC_CM`) | Read marker copy 1; if it fails to verify, read copy 2. On success apply the three-way test below |
| `0x5A` (`MAGIC_REC`) | Decode the header, fold the record into the scratch chain, and push `(seq, offset)` onto the pending batch |
| anything else | Stop |

A marker is accepted only if **all three** hold: it verifies under $k_{cm}$;
its `seq_max` equals the last sequence number of the pending batch; and its
`chi` equals the scratch chain value; and additionally its `epoch` is at least
the mounting epoch. On acceptance the scratch chain is promoted to the real
chain and every record in the pending batch is re-read, AEAD-opened, and applied
to the index in sequence order. On rejection the batch is discarded and the scan
stops — the discarded records were, by the acknowledgement rule, never
acknowledged.

If `open_record` fails for a record whose batch marker verified, the record is
skipped rather than panicking: boot **MUST NOT** abort the device with an
unwind. This is a transient-read-corruption path, not a normal one.

Cost is linear in the replayed tail and independent of the total volume stored.
Measured: $12.02$ flash pages read per replayed record with $R^2 = 0.999993$,
while a $44.8\times$ increase in stored volume raises mount reads only
$1.33\times$ and then saturates
([Section 6.12](#612-mount-cost-host)). The residual growth is checkpoint
loading, not replay.

Replay is the one engine path still on the blocking `Flash` trait and contains
no yield point, so it is a single uninterruptible span. This is a known
deviation, quantified in [Section 6.14](#614-asynchrony) and listed in
[Section 7.5](#75-standing-gaps).

### 3.8 Garbage collection

Compaction reclaims a sealed segment. `gc::compact_one_async`:

1. picks a victim with `pick_victim`, which **MUST NOT** select a segment at or
   above the checkpoint's `seg_seq` (a segment the current checkpoint still
   depends on) and **MUST NOT** select an open head;
2. scans the victim's records, yielding every `GC_YIELD_EVERY_RECORDS` $= 8$
   records;
3. for each record, resolves the key through the index and determines liveness;
4. relocates live records to the cold head, counting them in `gc_bytes`;
5. erases the victim's 12 blocks, yielding once per erase, and resets the
   segment entry to `Free`.

A record the scan cannot AEAD-open is counted in `gc_open_failed`. That counter
is surfaced deliberately: a nonzero value during reclaim means records were
treated as garbage without being read, which is data loss rather than a
statistic.

Segment-level RS(12,8) parity is encoded at seal time over the 8 data blocks.
Reconstruction recovers any pattern of up to 4 *declared* erasures exactly, and
refuses patterns beyond the code distance. This is verified exhaustively rather
than sampled: all 1,586 patterns for $e \le 5$
([Section 6.5](#65-erasure-coding-pure-computation)).

The guarantee is conditional on the driver declaring which blocks failed. When
blocks are corrupted *without* being declared, the code cannot identify them and
reconstruction produces wrong bytes — expected behaviour for an erasure code
as opposed to an error-correcting code. The authenticated-encryption layer is
what stops the wrong bytes reaching the application: measured, 8 of 12
single-block and 60 of 66 two-block undeclared-corruption cases failed to open.

**Reclaimed space is not currently reusable.** GC demonstrably works — the
device's erase trajectory decomposes exactly into checkpoint and segment
reclamation ([Section 6.15](#615-device-run-esp32-c3)) — but the log head
cannot wrap into freed segments, so a device halts with `FlashFull` while
almost all of its segments are free. See
[Section 7.5](#75-standing-gaps).

### 3.9 Error taxonomy

The core error type is closed and small:

| Variant | Meaning |
|---|---|
| `Tampered` | Authentication failed: AEAD open, marker HMAC, or counter MAC |
| `Rollback` | Boot rule: checkpoint epoch below the counter tip |
| `TornTail` | A partially written tail was detected and truncated |
| `BatchFull` | The batch buffer cannot hold another record; caller must commit |
| `FlashFull` | No space for the next append and no reusable free space reachable |
| `WearOut` | Erase failed and the block is unusable |
| `CounterExhausted` | The monotonic counter's increment budget is spent |
| `Io` | Driver-level read/program/erase failure |
| `FormatError` | A structure failed to decode: bad magic, bad lengths, bad geometry |
| `IndexFull` | Cuckoo insertion failed after `MAX_KICKS` and the stash is full |

Mount surfaces a narrower set (`Rollback`, `Tampered`, `FormatError`, `Io`),
which is what the tamper matrix of
[Section 6.6](#66-at-rest-tampering-host-and-simulated) exercises.

### 3.10 Security modes

The mode reported by a mounted volume reflects the *counter*, not the engine:

| `CounterKind` | `SecurityMode` | Meaning |
|---|---|---|
| `Hardware` | `Full` | eFuse, RPMC-backed flash, or a TEE-held counter: the freshness tip cannot be rolled back with the flash image |
| `BestEffort` | `BestEffortRollback` | A counter file on a general-purpose filesystem: it can be snapshotted and restored *together with* the volume, so monotonicity is only as strong as the surrounding system |
| `None` | `NoRollbackProtection` | No tip; the boot rule's freshness check is skipped entirely |

The boot rule executed is byte-identical in the `Hardware` and `BestEffort`
modes; only the strength of the tip differs. This is deliberate honest
degradation: the std `FileCounter` reports `BestEffort` and the volume reports
`BestEffortRollback`, rather than claiming a guarantee the platform cannot
support.

One reporting defect exists here: immediately after a genesis format the mode is
hardcoded to `BestEffortRollback` regardless of counter kind, and becomes
correct only on the first remount. See
[Section 7.4](#74-defect-4-genesis-security-mode-misreport).

---

## 4. Configuration constants and derived sizes

All constants below are from `crates/slate-kv-core/src/config.rs` at revision
`970324f` unless another file is named. Constants marked **format** fix the
on-flash layout: changing one invalidates existing volumes and requires a
reformat.

### 4.1 Format constants

| Constant | Value | Units | Kind | Notes |
|---|---|---|---|---|
| `MAGIC_REC` | `0x5A` | byte | format | Record header |
| `MAGIC_CM` | `0x5C` | byte | format | Commit marker |
| `MAGIC_SEG` | `0x51` | byte | format | Segment header (never written) |
| `MAGIC_XOR` | `0x58` | byte | format | XOR parity page |
| `MAGIC_CKPT` | `0xCF` | byte | format | Checkpoint header |
| `ERASED_BYTE` | `0xFF` | byte | format | Erased flash |
| `OP_PUT` | `0x00` | byte | format | |
| `OP_DEL` | `0x01` | byte | format | Tombstone |
| `REC_HDR_LEN` | 28 | bytes | format | Plaintext record header, AEAD associated data |
| `TAG_LEN` | 16 | bytes | format | Poly1305 tag |
| `REC_OVERHEAD` | 44 | bytes | format | `REC_HDR_LEN + TAG_LEN` |
| `CM_LEN` | 83 | bytes | format | Commit marker, HMAC included |
| `CKPT_HDR_LEN` | 76 | bytes | format | Checkpoint header, AEAD associated data |
| `CHI_LEN` | 32 | bytes | format | Chain value width (SHA-256) |
| `EPOCH_ANCHOR_TAG` | `"slate/epoch"` | — | format | Chain re-anchor domain separator |
| `SEG_BLOCKS_DATA` | 8 | blocks | format | `RS_K` |
| `SEG_BLOCKS_PARITY` | 4 | blocks | format | `RS_M` |
| `SEG_BYTES` | 49,152 | bytes | format | $12 \times 4096$ |
| `CKPT_SLOTS` | 2 | slots | format | |
| `CKPT_BASE_BLOCK` | 2 | blocks | format | First checkpoint block |
| `MAX_CKPT_LEN` | 262,276 | bytes | format | Derived, see [Section 4.3](#43-derived-sizes) |
| `MAX_INDEX_SLOTS` | 65,536 | slots | format | Ties index capacity to checkpoint capacity |
| `OFF_BITS` | 24 | bits | format | Index offset width; caps the region at 16 MiB |
| `FP_BITS` | 8 | bits | format | Fingerprint width stored in an index slot |

### 4.2 Tunable constants

| Constant | Value | Units | Notes |
|---|---|---|---|
| `MAX_KEY_LEN` | 256 | bytes | `FormatError` above this |
| `MAX_VAL_LEN` | 1024 | bytes | `FormatError` above this |
| `B_COMMIT` | 27 | records | Compile-time default batch size |
| `B_MAX` | 128 | records | Upper bound on the batch dial |
| `MAX_PAGE_SIZE` | 512 | bytes | Sizes the stack page buffer |
| `MAX_SEGS` | 256 | segments | Format-level cap |
| `MAX_SEGMENTS` | 128 | segments | Segment-table cap (`gc.rs`); the binding one |
| `THETA` | 16,384 | records | Automatic epoch-seal cadence |
| `BUCKET_SLOTS` | 4 | slots | Cuckoo bucket width |
| `STASH_SIZE` | 8 | entries | Cuckoo stash |
| `MAX_KICKS` | 500 | kicks | Cuckoo insertion attempts before `IndexFull` |
| `N_BUCKETS` | 2,048 | buckets | Compile-time default; the shipped ESP32 configuration |
| `GC_YIELD_EVERY_RECORDS` | 8 | records | Compaction scan yield cadence |
| `RECOVER_YIELD_EVERY_PAGES` | 32 | pages | **Declared but referenced nowhere** — see [Section 7.3](#73-defect-3-unimplemented-claims-in-the-async-design-document) |

`GC_YIELD_EVERY_RECORDS` is a plain `pub const u16` with no runtime override, so
sweeping it requires recompilation. The sweep in
[Section 6.14](#614-asynchrony) was performed that way.

### 4.3 Derived sizes

Every dependent size follows from the constants above by the arithmetic below.
All values are for the shipped ESP32-C3 configuration: $B_{blk} = 4096$ B,
$P = 256$ B, region $= 2$ MiB $= 2{,}097{,}152$ B, `N_BUCKETS` $= 2048$.

**Index arena.** One slot is a packed `u32` holding an 8-bit fingerprint and a
24-bit offset:

$$\text{arena} = \texttt{N\_BUCKETS} \cdot \texttt{BUCKET\_SLOTS} \cdot 4 = 2048 \cdot 4 \cdot 4 = 32{,}768 \text{ B}$$

$$n_{slots} = \texttt{N\_BUCKETS} \cdot \texttt{BUCKET\_SLOTS} = 8192$$

**Keys at the design load factor.** At $\alpha = 0.95$:

$$n_{keys} = \lfloor 0.95 \cdot 8192 \rfloor = 7782$$

**Checkpoint payload.**

$$\text{ckpt\_len} = \texttt{CKPT\_HDR\_LEN} + 4 n_{slots} + 5 \cdot \texttt{STASH\_SIZE} + \texttt{TAG\_LEN} = 76 + 32768 + 40 + 16 = 32{,}900 \text{ B}$$

**Format ceiling on the checkpoint.**

$$\texttt{MAX\_CKPT\_LEN} = 76 + 4 \cdot 65536 + 40 + 16 = 262{,}276 \text{ B}$$

**Checkpoint region and log base.**

$$\text{blocks per slot} = \lceil 262276 / 4096 \rceil = 65, \qquad \text{data\_base} = (2 + 2 \cdot 65) \cdot 4096 = 540{,}672 \text{ B}$$

**Segment count and log extent.**

$$N_{seg} = \left\lfloor \frac{2097152 - 540672}{49152} \right\rfloor = 31, \qquad \text{seg\_end} = 540672 + 31 \cdot 49152 = 2{,}064{,}384 \text{ B}$$

**Usable log bytes** (device run, `device_c3_analysis.json`): 1,556,480 B.

**Framed record length**, 12-byte key and 18-byte value:

$$L_{rec} = 44 + 12 + 18 = 74 \text{ B}$$

**Marker cost per commit** at $P = 256$: $2 \times 256 = 512$ B.

**Fingerprint collision bound.** For $b$ slots per bucket and $f$ fingerprint
bits, the probability that a lookup's candidate set contains an unrelated entry
is bounded by

$$\Pr[\text{fp collision}] \le 2b \cdot 2^{-f} = 2 \cdot 4 \cdot 2^{-8} = 0.03125$$

### 4.4 RAM working set

The engine's whole RAM cost is a sum of compile-time constants. The table below
is `size_of` measured on `riscv32imc-unknown-none-elf` (32-bit pointers,
matching the ESP32-C3 build) for the struct terms, and `llvm-nm` symbol sizes
for the firmware's static buffers. Source:
`docs/proposal/data/ram_working_set.csv`.

| Term | Bytes | Scales with `n_buckets` | Resident | Source |
|---|---:|:---:|:---:|---|
| Index arena | 32,768 | yes | yes | `N_BUCKETS`$\cdot$`BUCKET_SLOTS`$\cdot 4$ |
| Checkpoint buffer (required) | 32,900 | yes | yes | `ckpt_len_for_slots(8192)` |
| Checkpoint buffer (as built in `kv_demo`) | 35,012 | yes | yes | `CKPT_BUF: [u8; 35000]`, `llvm-nm` size incl. alignment |
| Hot batch buffer | 4,108 | no | yes | `HOT_BUF: [u8; 4096]` |
| Cold batch buffer | 4,108 | no | yes | `COLD_BUF: [u8; 4096]` |
| `ScratchWorkspace` | 5,720 | no | yes | Inline field of `Slate` (GC/candidate record buffers, page buffer) |
| `SegTable` | 3,088 | no | yes | Inline field of `Slate` |
| `EngineState` | 96 | no | yes | Inline field of `Slate` |
| `Index` struct excl. arena | 80 | no | yes | Slots are a `&mut` slice; stash inline |
| `Log` $\times 2$ excl. buffers | 64 | no | yes | Hot and cold |
| `Scheduler` | 64 | no | yes | |
| `Metrics` | 96 | no | yes | With the `metrics` feature enabled |
| `RecoverWorkspace` | 4,672 | no | **no** | `Box`ed in `Db::open`, dropped when `open` returns |

Exactly one of the two checkpoint-buffer rows is summed, never both.

$$\text{resident (required)} = 32768 + 32900 + 4108 + 4108 + 5720 + 3088 + 96 + 80 + 64 + 64 + 96 = 83{,}092 \text{ B} = 81.14 \text{ KiB}$$

$$\text{resident (as built)} = 85{,}204 \text{ B} = 83.21 \text{ KiB}, \qquad \text{mount peak} = 83092 + 4672 = 87{,}764 \text{ B} = 85.71 \text{ KiB}$$

The documented core working-set budget is 64 KiB $= 65{,}536$ B. The shipped
configuration exceeds it by

$$\frac{83092}{65536} - 1 = 26.8\%\ \text{resident}, \qquad \frac{87764}{65536} - 1 = 33.9\%\ \text{at the mount peak}$$

This is a **known deviation**, recorded in
[Section 7.5](#75-standing-gaps). The dominant unexpected term is the
checkpoint buffer: it must hold the entire serialized index, so at 32,900 B it
costs as much as the arena itself and roughly doubles the index's true memory
price. Any accounting that counts the arena and forgets the buffer understates
the engine by a factor of two.

An independent cross-check comes from the linked firmware: `llvm-nm` on
`kv_demo` reports 76,008 B of SLATE static buffers in `.data`
($4108 + 4108 + 32780 + 35012$), which is the four buffer rows above with the
as-built checkpoint buffer.

**Configurations against the 64 KiB budget** (same arithmetic swept over table
size, `ram_working_set.csv` Table 3):

| `n_buckets` | `n_slots` | Keys at $\alpha = 0.95$ | Arena (B) | Ckpt (B) | Resident (B) | Resident (KiB) | Fits 64 KiB | Mount peak (B) | Peak fits |
|---:|---:|---:|---:|---:|---:|---:|:---:|---:|:---:|
| 128 | 512 | 486 | 2,048 | 2,180 | 21,652 | 21.14 | yes | 26,324 | yes |
| 256 | 1,024 | 972 | 4,096 | 4,228 | 25,748 | 25.14 | yes | 30,420 | yes |
| 512 | 2,048 | 1,945 | 8,192 | 8,324 | 33,940 | 33.14 | yes | 38,612 | yes |
| 1,024 | 4,096 | 3,891 | 16,384 | 16,516 | 50,324 | 49.14 | yes | 54,996 | yes |
| **2,048** | **8,192** | **7,782** | **32,768** | **32,900** | **83,092** | **81.14** | **no** | **87,764** | **no** |
| 4,096 | 16,384 | 15,564 | 65,536 | 65,668 | 148,628 | 145.14 | no | 153,300 | no |

The largest configuration meeting the stated budget is `n_buckets` $= 1024$ at
49.14 KiB resident, supporting roughly 3,891 keys. Either the budget or the
shipped configuration must change; the measurement does not decide which.

One constraint on that choice: `Db::open` sizes the arena as
`next_power_of_two(max(n_keys, 2048) / 0.95) * BUCKET_SLOTS`, so the std build's
floor is also `n_buckets` $= 2048$. The smaller rows above are reachable on the
bare-metal target, which sizes `INDEX_SLOTS` directly, but **not** through
`Db::open` at this revision.

### 4.5 Firmware static footprint

Static ELF sizes for the four ESP32-C3 binaries, `llvm-size -A` on the release
cross-build with features `chip-esp32c3,counter-flash,metrics`. Source:
`docs/proposal/data/firmware_size.csv`.

| Binary | Links engine | `.text` | `.rodata` | `.data` | `.bss` | Flash resident | SLATE static bufs | `.data` excl. bufs |
|---|:---:|---:|---:|---:|---:|---:|---:|---:|
| `kv_demo` | yes | 100,898 | 15,648 | 77,756 | 456 | 194,302 | 76,008 | 1,748 |
| `embassy_demo` | yes | 91,530 | 14,076 | 77,696 | 456 | 183,302 | 76,008 | 1,688 |
| `slate_node` | yes | 91,538 | 13,972 | 77,580 | 456 | 183,090 | 76,008 | 1,572 |
| `bench` | **no** | 24,204 | 7,452 | 532 | 396 | 32,188 | 0 | 532 |

Two cautions, both normative for anyone quoting these figures.

`bench` links **no SLATE engine code** (`links_slate_engine = 0`): it is a bare
`esp-hal` boot stub that prints and spins. Its 532 B of `.data` **MUST NOT** be
cited as a lean SLATE configuration; it is the footprint of an empty program.

These are static ELF sizes, not runtime measurements. `.stack` in the linker
script is a *reservation* for the region, not measured stack usage, and heap and
stack high-water marks were not measured — no ESP32-C3 hardware was attached in
the session that produced this file. The device *behaviour* figures of
[Section 6.15](#615-device-run-esp32-c3) come from a separate, genuine hardware
run.

---

## 5. Rust API surface

The signatures below are reproduced from the source at revision `970324f`. Doc
comments are abridged; the contracts stated in prose around each block are
normative.

### 5.1 `slate-kv-hal`: the driver seam

An integrator supplies these. This is the entire hardware dependency of the
engine.

```rust
/// Program-once-per-erase NOR flash with detectable tearing.
pub trait Flash {
    type Error: core::fmt::Debug;

    /// Program granularity.
    fn page_size(&self) -> usize;
    /// Erase granularity; MUST be a multiple of page_size().
    fn block_size(&self) -> usize;
    /// Capacity in bytes. MUST be <= 1 << OFF_BITS (16 MiB).
    fn capacity(&self) -> u32;

    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error>;

    /// May only program pages that are in erased state. `addr` and `buf.len()`
    /// must be page-aligned / page-sized multiples. Completion = durable.
    fn program(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error>;

    /// Erases a block, resetting it to all 1s.
    fn erase(&mut self, block_addr: u32) -> Result<(), Self::Error>;
}
```

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CounterKind { Hardware, BestEffort, None }

/// Hardware monotonic counter, honest-degradation aware.
pub trait MonotonicCounter {
    type Error: core::fmt::Debug;

    fn kind(&self) -> CounterKind;
    fn read(&mut self) -> Result<u64, Self::Error>;

    /// MUST be durable before returning Ok. Fails when the budget is exhausted.
    fn increment(&mut self) -> Result<u64, Self::Error>; // returns new value
}

pub trait EntropySource {
    fn fill(&mut self, buf: &mut [u8]);
}

/// Optional coarse clock for the deadline clamp; ticks in ms.
pub trait Clock {
    fn now_ms(&self) -> u64;
}
```

The async twins are method-for-method identical, including the
program-once-per-erase contract and the `Error` associated type. They use
return-position `impl Future` rather than `async fn` in the trait, so they are
object-unsafe but allocation-free:

```rust
pub trait AsyncFlash {
    type Error: core::fmt::Debug;

    /// Never suspends — pure metadata.
    fn page_size(&self) -> usize;
    fn block_size(&self) -> usize;
    fn capacity(&self) -> u32;

    fn read(&mut self, addr: u32, buf: &mut [u8])
        -> impl core::future::Future<Output = Result<(), Self::Error>>;

    /// MUST fail if the target page was already programmed since its last erase.
    fn program(&mut self, addr: u32, buf: &[u8])
        -> impl core::future::Future<Output = Result<(), Self::Error>>;

    /// Erases one block. Indivisible: a caller can never observe a half-erased
    /// block, and cancelling the returned future does NOT abort an erase already
    /// latched into the die.
    fn erase(&mut self, block_addr: u32)
        -> impl core::future::Future<Output = Result<(), Self::Error>>;
}

pub trait AsyncMonotonicCounter {
    type Error: core::fmt::Debug;

    fn kind(&self) -> CounterKind;
    fn read(&mut self) -> impl core::future::Future<Output = Result<u64, Self::Error>>;
    /// Increments and returns the NEW value. Durable on return.
    fn increment(&mut self) -> impl core::future::Future<Output = Result<u64, Self::Error>>;
}
```

Two newtype bridges lift a blocking driver into the async traits by returning
already-ready futures, which is what keeps every existing board crate compiling
unchanged:

```rust
pub struct BlockingFlash<F: Flash>(pub F);
pub struct BlockingCounter<C: MonotonicCounter>(pub C);
```

The reverse direction also exists (an `AsyncFlash` driven to completion under
`block_on` presents as `Flash`). The cost of that projection is measured in
[Section 6.14](#614-asynchrony) and is *not* zero when the driver actually
suspends.

### 5.2 `slate-kv-core`: the crypto seam

`Sealer` is the whole cryptographic dependency. An integrator may substitute an
implementation — for a hardware AES engine, say — provided it preserves the
authenticated-encryption and MAC properties the recovery rules rely on.

```rust
pub trait Sealer {
    /// Seals a record; `hdr` is the AEAD associated data.
    fn seal_record(&mut self, hdr: &[u8; REC_HDR_LEN], plain_kv: &[u8], ct_tag_out: &mut [u8]);

    /// Opens a record. MUST return Err(Tampered) rather than plaintext on
    /// authentication failure.
    fn open_record(
        &mut self,
        hdr: &[u8; REC_HDR_LEN],
        ct_tag: &[u8],
        plain_out: &mut [u8],
    ) -> Result<(), Error>;

    fn commit_marker(
        &mut self,
        seq_max: u64,
        epoch: u64,
        xor_pages: u16,
        chi: &[u8; 32],
    ) -> [u8; CM_LEN];
    fn verify_marker(&self, cm: &[u8; CM_LEN]) -> Result<CmFields, Error>;

    fn seal_checkpoint(&mut self, epoch: u64, slot: u8, ad: &[u8], in_out: &mut [u8]) -> [u8; 16];
    fn open_checkpoint(
        &mut self,
        epoch: u64,
        slot: u8,
        ad: &[u8],
        in_out: &mut [u8],
        tag: &[u8; 16],
    ) -> Result<(), Error>;

    /// Rotates the epoch key.
    fn roll_epoch(&mut self, e: u64);
}
```

The shipped implementation is `slate_kv_crypto::CryptoSealer`:
ChaCha20-Poly1305 for records and checkpoints, HMAC-SHA256 for commit markers
and segment headers, HKDF-SHA256 for the key schedule
([Section 2.5](#25-nonce-and-key-schedule)). `DeviceKey` and `KeySet` are
`ZeroizeOnDrop` and their `Debug` implementations print `<REDACTED>`.

### 5.3 `slate-kv-core`: the engine

`Slate` is the engine proper. There is exactly **one** `impl` block, generic
over the async traits; the async methods are the only implementations of every
algorithm.

```rust
pub struct Slate<'a, F, C, S> { /* ... */ }

impl<'a, F: slate_kv_hal::AsyncFlash,
         C: slate_kv_hal::AsyncMonotonicCounter,
         S: Sealer> Slate<'a, F, C, S>
{
    // --- RAM-only, correctly synchronous ---
    pub fn append_hot(&mut self, op: u8, key: &[u8], val: &[u8]) -> Result<u32, Error>;
    pub fn cold_batch_full(&self) -> bool;
    pub fn index_points_to(&self, key_candidates: &[&[u8]], offset: u32) -> bool;
    pub fn next_commit_deadline_ms(&self, now_ms: u64) -> Option<u64>;

    // --- async: the only implementations ---
    pub async fn get_into_async(&mut self, key: &[u8], out: &mut [u8]) -> Option<usize>;
    pub async fn index_update_offset_async(&mut self, key: &[u8], new_off: u32) -> Result<(), Error>;
    pub async fn index_remove_key_async(&mut self, key: &[u8]) -> bool;
    pub async fn append_cold_async(&mut self, key: &[u8], val: &[u8], now_ms: u64) -> Result<u32, Error>;
    pub async fn append_cold_tombstone_async(&mut self, key: &[u8], now_ms: u64) -> Result<(), Error>;
    pub async fn commit_async(&mut self) -> Result<(), Error>;
    pub async fn seal_epoch_now_async(&mut self) -> Result<(), Error>;
    pub async fn compact_async(&mut self) -> Result<(), Error>;

    // --- blocking projection: each body is a single block_on call ---
    #[cfg(feature = "blocking")] pub fn get_into(&mut self, key: &[u8], out: &mut [u8]) -> Option<usize>;
    #[cfg(feature = "blocking")] pub fn index_update_offset(&mut self, key: &[u8], new_off: u32) -> Result<(), Error>;
    #[cfg(feature = "blocking")] pub fn index_remove_key(&mut self, key: &[u8]) -> bool;
    #[cfg(feature = "blocking")] pub fn append_cold(&mut self, key: &[u8], val: &[u8], now_ms: u64) -> Result<u32, Error>;
    #[cfg(feature = "blocking")] pub fn append_cold_tombstone(&mut self, key: &[u8], now_ms: u64) -> Result<(), Error>;
    #[cfg(feature = "blocking")] pub fn commit(&mut self) -> Result<(), Error>;
    #[cfg(feature = "blocking")] pub fn seal_epoch_now(&mut self) -> Result<(), Error>;
    #[cfg(feature = "blocking")] pub fn compact(&mut self) -> Result<(), Error>;
}
```

All eight blocking wrappers have a one-line body of the form
`crate::task::block_on(self.X_async(..))`. This was verified programmatically
(`docs/proposal/data/async_facade.json`), which is what justifies treating the
two façades as behaviourally identical: there is no second implementation that
could drift.

The free functions follow the same pattern:

```rust
pub async fn seal_epoch_async<F: AsyncFlash, C: AsyncMonotonicCounter>(/* ... */) -> Result<(), Error>;
#[cfg(feature = "blocking")] pub fn seal_epoch<F: Flash, C: MonotonicCounter>(/* ... */) -> Result<(), Error>;

pub async fn mount_async<F: AsyncFlash, C: AsyncMonotonicCounter>(
    flash: &mut F, ctr: &mut C, s: &mut impl Sealer, out_buf: &mut [u8],
) -> Result<MountInfo, MountError>;
#[cfg(feature = "blocking")] pub fn mount<F: Flash, C: MonotonicCounter>(/* ... */) -> Result<MountInfo, MountError>;

pub async fn compact_one_async</* ... */>(/* ... */) -> Result<(), Error>;

/// BLOCKING ONLY at this revision — no async form exists.
pub fn recover<F: Flash, S: Sealer>(
    flash: &mut F,
    s: &mut S,
    chain: &mut Chain,
    epoch: u64,
    start_off: u32,
    workspace: &mut RecoverWorkspace,
    apply: impl FnMut(&mut F, &mut S, u64, u32, u8, &[u8]),
) -> Result<RecoverInfo, Error>;
```

With `blocking` **off**, `epoch.rs` re-exports the async forms under the short
names (`pub use seal_epoch_async as seal_epoch;`). A caller therefore sees a
name whose type changes with a feature flag; this is documented here because it
surprises integrators.

Asymmetries in the projection, all measured
(`docs/proposal/data/async_facade.json`):

| Operation | Available in | Note |
|---|---|---|
| `recover::recover` | blocking only | Also `record_key_eq`, `scan_segment_headers` |
| `repair::scrub` | blocking only | Body is a stub returning `Ok(())` |
| `gc::compact_one_async` | async only | Sync callers must go through `Slate::compact` |
| `segment::encode_parity` | async only | No caller anywhere in the workspace |
| `Slate::append_hot` | sync only | Correct: it only touches the RAM batch |
| `SlateSync` newtype | does not exist | Specified by the design document; never implemented |

### 5.4 The index

```rust
/// 64-bit FNV-1a.
pub fn h64(key: &[u8]) -> u64;
/// Top byte of the FNV-1a hash.
pub fn fingerprint(key: &[u8]) -> u8;
pub fn bucket1(h: u64, n: usize) -> usize;
pub fn alt_bucket(i: usize, fp: u8, n: usize) -> usize;

pub struct Index<'a> { /* slots: &'a mut [u32], stash, len */ }

impl<'a> Index<'a> {
    pub fn new(slots: &'a mut [u32], n_buckets: usize) -> Self;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;

    /// Collects candidate offsets from both buckets and the whole stash.
    pub fn candidates(&self, key: &[u8], out: &mut CandidateBuf);
    /// As `candidates`, additionally returning how many slots were probed.
    pub fn candidates_probed(&self, key: &[u8], out: &mut CandidateBuf) -> usize;
    pub fn probe_occupancy(&self, key: &[u8]) -> (usize, usize);
    pub fn stash_occupancy(&self) -> usize;

    pub fn upsert(/* ... */);
    pub fn remove(&mut self, key: &[u8], matches_key: impl FnMut(u32) -> bool) -> bool;

    pub fn serialize(&self, out: &mut [u8]) -> usize;
    pub fn deserialize(&mut self, data: &[u8]);
}
```

The arena is borrowed, not owned: `Index<'a>` holds `&'a mut [u32]`, which is
what lets the bare-metal target place the arena in a `static` and the std build
place it in a `Box` without two implementations. `serialize`/`deserialize` are
the checkpoint path.

Because `candidates` scans both buckets and the entire stash unconditionally,
the probe count is exactly 16 for every lookup — this is a constant, not a
bound that happens to be respected ([Section 6.11](#611-index-behaviour-in-ram)).

### 5.5 `slate-kv`: the std wrapper

```rust
pub use db::{Db, DbError, KeySource, MountReport, Options, Profile, ScrubReport, Stats};

#[derive(Clone, Debug)]
pub struct Options {
    pub capacity: u32,
    pub b_commit: u32,
    pub auto_b: bool,
    pub staleness_budget_ms: u32,
    pub n_keys: usize,
    pub profile: Profile,          // Esp32 | Pi
    pub durability: Durability,    // Full | OsCache
}

impl Default for Options {
    fn default() -> Self {
        Self {
            capacity: 4 * 1024 * 1024,
            b_commit: 8,
            auto_b: true,
            staleness_budget_ms: 1000,
            n_keys: 8192,
            profile: Profile::Pi,
            durability: Durability::Full,
        }
    }
}

pub enum KeySource { Bytes(/* [u8; 32] */), File(/* PathBuf */), Env(/* String */) }
```

```rust
impl Db {
    pub fn open(path: &Path, key: KeySource, opts: Options) -> Result<Self, DbError>;

    pub fn put(&self, key: &[u8], val: &[u8]) -> Result<(), DbError>;
    pub fn put_durable(&self, key: &[u8], val: &[u8]) -> Result<(), DbError>;
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, DbError>;
    pub fn delete(&self, key: &[u8]) -> Result<(), DbError>;
    pub fn delete_durable(&self, key: &[u8]) -> Result<(), DbError>;
    pub fn commit(&self) -> Result<(), DbError>;

    pub fn compact(&self) -> Result<(), DbError>;
    pub fn seal_epoch(&self) -> Result<(), DbError>;
    pub fn scrub(&self) -> Result<ScrubReport, DbError>;

    pub fn security_mode(&self) -> SecurityMode;
    pub fn acked_seq(&self) -> u64;
    pub fn next_seq(&self) -> u64;
    pub fn epoch(&self) -> u64;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn stats(&self) -> Stats;
    pub fn mount_report(&self) -> MountReport;
}
```

Note that the mutating methods take `&self`. The engine state sits behind
interior mutability, which enforces single-instance access *within* a process
but not across processes ([Section 1.3](#13-system-model)).

`Durability::Full` maps to `fcntl(F_FULLFSYNC)` on Darwin and `OsCache` to
`File::sync_data()`. On macOS these are **indistinguishable in cost**, so the
option is not a durability trade-off this platform can demonstrate; see
[Section 6.8](#68-host-flash-barrier-calibration). On Linux `flush_durable`
takes the `#[cfg(not(target_os = "macos"))]` branch and calls `sync_data` for
both, so they are identical there by construction.

`Options::n_keys` does not translate directly into an arena size: `Db::open`
computes `next_power_of_two(max(n_keys, 2048) / 0.95) * BUCKET_SLOTS`, so
`n_buckets` $< 2048$ is unreachable through this API
([Section 4.4](#44-ram-working-set)).

### 5.6 Observability: `MountReport` and `Stats`

Wall-clock alone cannot distinguish an $O(\Theta)$ tail replay from a
full-volume scan that happens to be fast, so mount is instrumented to separate
the terms. This is the interface the recovery measurements read.

```rust
#[derive(Debug, Default, Clone, Copy)]
pub struct MountReport {
    pub had_checkpoint: bool,      // false = this open formatted the volume
    pub replay_from: u32,          // first log byte the tail scan started from
    pub head_pos: u32,             // log head after replay
    pub scan_bytes: u32,           // bytes of log the tail scan walked
    pub records_replayed: u64,     // committed records AEAD-opened into the index
    pub ckpt_index_bytes: usize,   // serialized index bytes loaded (0 if none)
    pub keys: usize,
    pub index_slots: usize,
    pub ckpt_slots_verified: u8,   // bounded by CKPT_SLOTS
    pub flash: FlashCounters,      // read at the HAL boundary
    pub flash_after_ckpt: FlashCounters,
    pub key_verify_calls: u64,     // full-key AEAD verifications to resolve fp collisions
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FlashCounters {
    pub read_ops: u64, pub read_bytes: u64, pub read_pages: u64,
    pub program_ops: u64, pub program_bytes: u64, pub erase_ops: u64,
}
```

### 5.7 Metrics bucket definitions

Write amplification is only meaningful if every byte is attributed, so the
engine classifies each byte it programs into exactly one of five buckets. The
definitions are normative — a change to what a bucket counts changes what
reported amplification means.

```rust
#[cfg(feature = "metrics")]
pub struct Metrics {
    pub commits: u64,
    pub wakes: u64,
    /// Record bytes the application asked to store (framing + key + value).
    pub user_bytes: u64,
    /// Record bytes rewritten by GC relocation.
    pub gc_bytes: u64,
    /// XOR/RS parity pages programmed.
    pub parity_bytes: u64,
    /// Commit-marker pages programmed (two copies per commit).
    pub marker_bytes: u64,
    /// Checkpoint pages programmed by epoch seals.
    pub ckpt_bytes: u64,
    pub erases: u64,
    /// Records visited by a compaction scan.
    pub gc_scanned: u64,
    /// Records the scan found still live and relocated.
    pub gc_relocated: u64,
    /// Records the scan could not AEAD-open. Nonzero during reclaim means
    /// records were treated as garbage without being read — data loss, not a
    /// statistic. Surfaced deliberately.
    pub gc_open_failed: u64,
    pub gc_segments_freed: u64,
}
```

$$\text{flash\_bytes} = \texttt{user\_bytes} + \texttt{gc\_bytes} + \texttt{parity\_bytes} + \texttt{marker\_bytes} + \texttt{ckpt\_bytes}$$

$$\mathrm{WA} = \frac{\text{flash\_bytes}}{\texttt{user\_bytes}}$$

`write_amplification()` returns `Option`, and returns `None` rather than `1.0`
when `user_bytes == 0`: an unmeasured workload and a workload with no overhead
are different claims and **MUST NOT** be reported identically.

Three properties of this definition matter when reading
[Section 6.7](#67-write-amplification-host).

1. `user_bytes` counts the **framed** record (`REC_OVERHEAD + klen + vlen`), not
   the application's payload. Amplification relative to raw key-plus-value bytes
   is higher by the framing ratio.
2. `user_bytes` **MUST** be counted at exactly one call site. It was
   double-counted at this revision's predecessor; see
   [Section 7.1](#71-defect-1-user_bytes-double-counted-fixed).
3. The buckets count *logical* bytes handed to `program`, and records are packed
   into pages. Page padding is therefore attributed to **no** bucket, so
   reported amplification understates physical amplification. Quantified on the
   device in [Section 6.15](#615-device-run-esp32-c3): 22 B per record, an 11%
   understatement.

With the `metrics` feature off, every method is an `#[inline(always)]` no-op and
`write_amplification()` returns `None`, so a production build pays nothing.

### 5.8 C ABI

`slate-kv-ffi` exposes a flat C surface: `slate_abi_version`, `slate_open`,
`slate_put`, `slate_put_durable`, `slate_get`, `slate_delete`, `slate_commit`,
`slate_compact`, `slate_security_mode`, `slate_close`,
`slate_last_error_message`. A Go binding (719 lines) sits on top of it. Neither
is exercised by the measurements in this document beyond the FFI smoke test in
the suite.

---

## 6. Conformance and measured behaviour

Every subsection below names its platform, the exact command that produced the
data, and the data file. Commands are run from the workspace root.

Note on harness names: the measurement harnesses were renamed from `paper_*` to
`slate_*` after these data files were generated. The commands below use the
**current** names; the `command:` lines recorded inside the data-file headers
still carry the old ones and will not run as written.

### 6.1 Provenance

| Item | Value |
|---|---|
| Revision | `970324f20f3fc7d7df4249c4d51122f9a1a5e61c` (`970324f`), 2026-07-29 |
| Workspace version | 0.4.0 |
| Toolchain | `rustc` 1.97.1 (8bab26f4f 2026-07-14), `cargo` 1.97.1, edition 2021 |
| Host | macOS 26.5.2 (25F84), arm64, 12 cores, 24 GiB |
| Embedded target | `riscv32imc-unknown-none-elf` (ESP32-C3) |
| `no_std` check target | `thumbv7em-none-eabihf` |
| Firmware features | `chip-esp32c3,counter-flash,metrics` |
| `cargo fmt --check` | clean |
| `cargo clippy -D warnings` | clean |
| `no_std` core build | ok |
| Firmware build | ok, 4 binaries |
| `cargo test` | 63 passed, 0 failed, 1 ignored |

Source: `docs/proposal/data/provenance.json`,
`docs/proposal/data/testsuite.json`.

The working tree carried 9 modified paths relative to `970324f` when the
measurements were taken: the measurement harnesses listed in
[Section 9](#9-appendix-a-data-file-inventory), plus the byte-accounting fix of
[Section 7.1](#71-defect-1-user_bytes-double-counted-fixed).

**The single ignored test is `space_reuse_after_reclaim`**
(`crates/slate-kv/tests/esp32_defects.rs:199`), ignored with the reason
`"requires segment-aware head roll + on-flash segment headers"`. It asserts
exactly the property [Section 7.5](#75-standing-gaps) reports the format does
not yet provide. It is named here rather than quietly skipped.

Test distribution across the suite (`testsuite.json`): `slate-kv-core` unit 11,
`flash_region_layout` 6, `slate-kv-crypto` unit 10, `slate-kv-erasure` unit 3,
`slate-kv-hal` unit 2, `slate-kv-sim` unit 2, and the `slate-kv` integration
suites `kv_roundtrip` 5, `epoch_lifecycle` 4, `esp32_defects` 4 (+1 ignored),
`probe_ckpt_size` 3, `kill9` 1, `security` 1, `probe_cold_overlap` 1,
`probe_epoch` 1, `probe_epoch_key` 1, `probe_index_cap` 1.

### 6.2 Reproduction commands

| Data file | Command | Platform |
|---|---|---|
| `provenance.json` | environment capture (`git`, `rustc -V`, `cargo test`, `cargo fmt`, `clippy`, cross-builds) | host |
| `testsuite.json` | `cargo test --workspace` | host |
| `crash_mc.json` | `cargo run --release -p slate-kv-sim --bin crash_mc` | simulated |
| `erasure.csv` | `cargo run --release -p slate-kv-sim --example rs_exhaustive` | pure computation |
| `tamper.json` | `cargo run --release -p slate-kv-sim --example tamper_matrix` | host + simulated |
| `wa_buckets.csv` | `cargo run --release -p slate-kv --example slate_wa_buckets` | host (`FileFlash`) |
| `wa_study.csv` | `cargo run --release -p slate-kv-sim --bin wa_study_paper` | modelled (not the engine) |
| `wa_study_matched.csv` | `cargo run --release -p slate-kv-sim --bin wa_study_paper --capacity-sweep` | modelled |
| `wa_study_original.csv` | `cargo run --release -p slate-kv-sim --bin wa_study` | modelled (original harness) |
| `throughput.csv` `[per_run]`, `[summary_by_b_commit]` | `cargo run --release -p slate-kv --example slate_throughput` | host (`FileFlash`) |
| `throughput.csv` `[flash_barrier_calibration]` | `cargo run --release -p slate-kv --example slate_flash_calib` | host |
| `energy_batch.csv` | `cargo run --release -p slate-kv-sim --bin slate_energy_batch` | simulated traffic + modelled joules |
| `index_ram.csv` | `cargo run --release -p slate-kv-core --example slate_index` | pure in-RAM |
| `fp_remedy.csv` | `cargo run --release -p slate-kv-core --example slate_fp_remedy` | modelled, pure in-RAM |
| `recovery.csv` | `cargo run --release -p slate-kv --example slate_recovery` | host (`FileFlash`) |
| `ram_working_set.csv` | `size_of` probe crate cross-built for `riscv32imc`, read back with `llvm-objdump`; `llvm-nm` for static buffers | static analysis |
| `firmware_size.csv` | `cd targets/esp32 && cargo build --release --target riscv32imc-unknown-none-elf --features chip-esp32c3,counter-flash,metrics`, then `llvm-size -A` | static analysis |
| `async_future_size.csv` | `cargo run -q --release -p slate-kv-sim --example slate_async_future_size` | host |
| `async_yield.csv` | `cargo run -q --release -p slate-kv-sim --example slate_async_yield` | simulated latency model |
| `async_blocking_cost.csv` | `cargo run -q --release -p slate-kv-sim --example slate_async_blocking_cost` | host |
| `async_facade.json` | source reads, `grep`, `cargo tree`, `llvm-nm`, probe-crate compile matrix | static analysis |
| `device_c3.csv`, `device_c3_analysis.json` | ESP32-C3 serial log, `embassy_demo` firmware, 9 checkpoint reports | **device** |
| `user_bytes_bug.json` | ratio check plus post-fix regeneration of `wa_buckets.csv` | host + simulated |

### 6.3 Crash injection (simulated)

**Platform:** `slate-kv-sim` `SimFlash`, an in-RAM fault-injecting NOR simulator.
**Command:** `cargo run --release -p slate-kv-sim --bin crash_mc`.
**Data:** `crash_mc.json`.

Geometry: 1 MiB capacity, 256 B pages, 4 KiB blocks, `b_commit` $= 8$,
`auto_b` off, 1,024 keys, 600 records per trial.

Each trial writes a workload, cuts power at a **uniformly random byte offset
within a uniformly random flash operation**, remounts, and compares the
recovered state against a separately maintained ground-truth log. The cut is
uniform over bytes rather than over records, which places most cuts *inside* a
page program; the workload contains commits, garbage collection and checkpoint
writes, so cuts land in all three.

| Predicate | Trials | Violations |
|---|---:|---:|
| Recovered state is a prefix of the acknowledged sequence | 20,000 | **0** |
| No acknowledged write lost | 20,000 | **0** |
| No unacknowledged write accepted | 20,000 | **0** |

Campaign wall time 37.684 s.

### 6.4 Rollback resistance (simulated)

**Same harness and data file.** 5,000 splice attacks: an earlier-epoch data page
is AND-merged over the log tail — which is what a physical attacker can do to
NOR flash without erasing — and mount must still report the correct
acknowledged sequence number (`acked_seq == 32`).

| Attacks | Rejections | Rate |
|---:|---:|---:|
| 5,000 | 5,000 | 100% |

### 6.5 Erasure coding (pure computation)

**Platform:** pure-computation harness; no flash device involved.
**Command:** `cargo run --release -p slate-kv-sim --example rs_exhaustive`.
**Data:** `erasure.csv`.

The stripe is 8 data blocks packed with 27 real AEAD-sealed SLATE records (2,017
bytes of record data) plus 4 Cauchy parity blocks over $\mathrm{GF}(2^8)$ with
`GF_POLY` $= \texttt{0x11D}$. `recovered_exact` requires all 12 blocks
byte-identical to the encoded stripe **and** all 27 records re-openable under
AEAD — correctness at the cryptographic layer, not merely the byte layer.

Every pattern of every size was tested, not a sample: $\binom{12}{e}$ patterns
for each $e$.

**Declared erasures** (blocks zeroed *and* declared in the `BlockSet`):

| Blocks lost $e$ | Patterns | Recovered exactly | Refused | Wrong bytes | Singular survivor matrices |
|---:|---:|---:|---:|---:|---:|
| 0 | 1 | 1 | 0 | 0 | 0 |
| 1 | 12 | 12 | 0 | 0 | 0 |
| 2 | 66 | 66 | 0 | 0 | 0 |
| 3 | 220 | 220 | 0 | 0 | 0 |
| 4 | 495 | 495 | 0 | 0 | 0 |
| 5 | 792 | 0 | 792 | 0 | 0 |
| **Total** | **1,586** | **794** | **792** | **0** | **0** |

All 794 patterns within the code distance ($e \le 4$) reconstructed byte-exactly;
all 792 patterns at $e = 5$ were refused with an explicit error; no survivor
matrix was singular; and across all 1,586 patterns the harness observed **zero**
wrong bytes.

**Undeclared corruption** (blocks bit-flipped, `BlockSet` left empty) is a
different failure and is reported as such:

| Blocks corrupted $e$ | Patterns | Recovered exactly | Wrong bytes | Caught by AEAD |
|---:|---:|---:|---:|---:|
| 1 | 12 | 0 | 12 | 8 |
| 2 | 66 | 0 | 66 | 60 |

This is the expected behaviour of an erasure code as opposed to an
error-correcting code: with no declaration the decoder cannot identify which
blocks are bad. It is not a defect, but it makes the erasure guarantee
**conditional on the driver reporting which blocks failed**. The
authenticated-encryption layer is the backstop: 8 of the 12 single-block cases
and 60 of the 66 two-block cases failed to open, and the remainder were caught
by the record checksum. The reconstruction is wrong; the wrong data does not
reach the application.

### 6.6 At-rest tampering (host and simulated)

**Platform:** `FileFlash` file-backed emulation plus one in-RAM `SimFlash` probe.
**Command:** `cargo run --release -p slate-kv-sim --example tamper_matrix`.
**Data:** `tamper.json`.

Geometry: 8 MiB capacity, 256 B pages, 4 KiB blocks, `data_base` 540,672,
checkpoint slot 0 at 8,192 and slot 1 at 274,432, `THETA` 16,384,
`b_commit` $= 8$. Every volume has 48 distinct committed key/value records
before the attack. Each row records whether mount succeeded, how many
ground-truth keys still read back, and — the property that matters — whether
any read returned a value that was never written.

| Attack | Bytes changed | Outcome | Keys readable | Wrong values |
|---|---:|---|---:|---:|
| `control_no_attack_single_epoch` | 0 | Ok(mounted, security_mode=BestEffortRollback) | 48 / 48 | 0 |
| `control_no_attack_two_epochs` | 0 | Ok(mounted, security_mode=BestEffortRollback) | 48 / 48 | 0 |
| `control_no_attack_three_epochs` | 0 | Ok(mounted, security_mode=BestEffortRollback) | 48 / 48 | 0 |
| `record_ciphertext_body_bitflip_first` | 1 | Ok(mounted, security_mode=BestEffortRollback) | 0 / 48 | 0 |
| `record_ciphertext_body_bitflip_last` | 1 | Ok(mounted, security_mode=BestEffortRollback) | 40 / 48 | 0 |
| `record_header_bitflip` | 1 | Ok(mounted, security_mode=BestEffortRollback) | 40 / 48 | 0 |
| `record_aead_tag_bitflip` | 1 | Ok(mounted, security_mode=BestEffortRollback) | 40 / 48 | 0 |
| `log_truncated_mid_record` | 7,839,715 | Ok(mounted, security_mode=BestEffortRollback) | 40 / 48 | 0 |
| `commit_marker_copy1_body_bitflip` | 1 | Ok(mounted, security_mode=BestEffortRollback) | 48 / 48 | 0 |
| `commit_marker_copy1_zeroed` | 256 | Ok(mounted, security_mode=BestEffortRollback) | 40 / 48 | 0 |
| `commit_marker_both_copies_zeroed` | 512 | Ok(mounted, security_mode=BestEffortRollback) | 40 / 48 | 0 |
| `checkpoint_active_slot_bitflip` | 1 | Err(DbError::Mount(MountError::Rollback)) | — | — |
| `checkpoint_older_slot_bitflip` | 1 | Ok(mounted, security_mode=BestEffortRollback) | 48 / 48 | 0 |
| `checkpoint_both_slots_bitflip` | 2 | Err(DbError::Mount(MountError::Tampered)) | — | — |
| `rollback_replay_older_epoch_image` | 8,388,608 | Err(DbError::Mount(MountError::Rollback)) | — | — |
| `forward_spliced_image_epoch_gap` | 8,388,608 | Err(DbError::Mount(MountError::Tampered)) | — | — |
| `cross_epoch_record_splice` | 256 | Ok(mounted, security_mode=BestEffortRollback) | 48 / 48 | 0 |
| `counter_file_hmac_corrupt_both_slots` | 2 | Err(DbError::Mount(MountError::Tampered)) | — | — |

**Summary: 18 attacks, 0 unsafe outcomes, 0 wrong values returned.** Three rows
are pristine-volume controls. Of the 15 attacks, 5 were refused outright at
mount (`Rollback` twice, `Tampered` three times) and 10 mounted with the damage
contained by tail truncation, losing the affected suffix but preserving every
key committed before it.

Two findings from this matrix are specification-relevant.

*The twin commit marker protects less than its presence suggests.* Zeroing all
256 bytes of marker copy 1 — magic byte included — is **not** recovered by
falling back to copy 2, because the recovery scanner dispatches on the first
byte at the offset and so never reaches the verify-then-fall-back path
([Section 3.7](#37-tail-replay)). Corrupting a single bit of the marker *body*
is recovered (48 of 48 keys readable). The redundancy covers a tear inside the
body, not destruction of the leading byte.

*The two-slot checkpoint scheme cannot silently fall back either, and that is
correct.* A single bit flipped in the newest checkpoint yields `Rollback`, not a
silent revert to the older slot: the older slot carries a lower epoch than the
counter tip, so the boot rule rejects it. Redundancy covers a crash inside the
seal window, not deliberate corruption of the newest slot.

Also confirmed: `cross_epoch_record_splice` — relocating a page of the engine's
own authentic sealed records into the current epoch's replay tail — is dropped
even though the AEAD tags still verify, because the commit marker's chain value
does not match the records actually written there. This is the case that needs no
attacker key, and the chain is what catches it.

### 6.7 Write amplification (host)

**Platform:** host, real engine, `FileFlash`, ESP32-C3 geometry (2 MiB region,
256 B pages, 4 KiB blocks), `Durability::Full`.
**Command:** `cargo run --release -p slate-kv --example slate_wa_buckets`.
**Data:** `wa_buckets.csv`.

Workload: 100 B values, key `sensor_%06d` (13 B), 256 distinct keys, 6,000
operations, explicit `compact()` at the end. Percentages are of
`total_bytes` and are computed from the bucket columns; they sum to 100.00% on
every row.

| $B$ | Ops accepted | Commits | `user_bytes` | `total_bytes` | User % | Marker % | Parity % | Ckpt % | GC % | WA |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1464 | 1522 | 230,005 | 1,450,831 | 15.9 | 52.8 | 26.4 | 4.5 | 0.37 | **6.3078** |
| 2 | 2433 | 1281 | 382,138 | 1,381,818 | 27.7 | 45.1 | 22.5 | 4.8 | 0.00 | **3.6160** |
| 4 | 3955 | 1039 | 621,092 | 1,476,537 | 42.1 | 35.0 | 17.5 | 4.5 | 0.95 | **2.3773** |
| 8 | 6000 | 784 | 942,000 | 1,589,864 | 59.3 | 24.2 | 12.1 | 4.1 | 0.24 | **1.6878** |
| 16 | 6000 | 377 | 942,000 | 1,295,792 | 72.7 | 14.8 | 7.4 | 5.1 | 0.00 | **1.3756** |
| 27 | 6000 | 224 | 942,000 | 1,179,056 | 79.9 | 9.7 | 4.8 | 5.6 | 0.00 | **1.2517** |
| 32 | 6000 | 189 | 942,000 | 1,152,176 | 81.8 | 8.4 | 4.2 | 5.7 | 0.00 | **1.2231** |
| 64 | 6000 | 95 | 942,000 | 1,079,984 | 87.2 | 4.5 | 2.2 | 6.1 | 0.00 | **1.1465** |
| 128 | 6000 | 48 | 942,000 | 1,043,888 | 90.2 | 2.3 | 1.2 | 6.3 | 0.00 | **1.1082** |

**Commit markers dominate at small batches, not garbage collection.** Marker
bytes are 52.8% of everything written at $B = 1$ and 2.3% at $B = 128$.
Garbage-collection relocation never exceeds **0.95%** of bytes written anywhere
in the sweep (its maximum is at $B = 4$), and is 0.37% at $B = 1$. This inverts
the expectation the LSM and SSD literature sets up, where compaction is the
dominant amplification term.

The mechanism is page granularity. A marker is 83 logical bytes but occupies a
whole 256 B page, and two copies are written, so each commit costs 512 B
regardless of batch size ([Section 2.6](#26-commit-marker)). At $B = 1$ that is
paid once per record; at $B = 128$ it is amortised across 128.

Amplification falls from **6.3078** to **1.1082**, a $5.69\times$ reduction, with
most of the benefit realised by $B = 16$ (1.3756).

Parity is exactly half the marker bytes on every row, which is the RS(12,8) ratio
of [Section 2.3](#23-segment-geometry) applied to the same page-quantised data.
Checkpoint bytes are constant at 65,792 B across the sweep, as they must be: two
seals of a fixed-size index.

The practical consequence for a deployment: **durability granularity, not
compaction policy, is the primary lifetime lever on this class of device.** An
application that can tolerate losing 16 records on power failure writes roughly a
fifth of the flash bytes of one that cannot tolerate losing any.

![Write amplification](proposal/figures/fig2_write_amplification.png)

### 6.8 Host flash barrier calibration

**Platform:** host, `FileFlash` on APFS. **Command:**
`cargo run --release -p slate-kv --example slate_flash_calib`. **Data:**
`throughput.csv` section `[flash_barrier_calibration]`, 300 programs per mode.

| Operation | Mode | Mean (µs) | p50 (µs) | p90 (µs) | p99 (µs) |
|---|---|---:|---:|---:|---:|
| `FileFlash::program` | Full | 8249.4 | 8005.3 | 9955.0 | 14835.5 |
| `FileFlash::program` | OsCache | 8363.0 | 8010.6 | 9924.0 | 12454.7 |
| `barrier_only` | rust_File::sync_data | 10616.4 | 8024.0 | 11911.9 | 19099.1 |
| `barrier_only` | libc_fsync | 519.7 | 185.5 | 382.4 | 5623.5 |
| `barrier_only` | libc_fcntl_F_FULLFSYNC | 13636.9 | 11755.8 | 21719.8 | 29805.2 |
| `raw_pwrite` | no_barrier | 265.4 | 3.1 | 7.9 | 4201.7 |

**The two `Durability` modes are indistinguishable on this platform**: 8,005 µs
versus 8,010 µs at the median per 256 B page. `Durability::OsCache` calls Rust's
`File::sync_data()`, which on Darwin costs the same as `fcntl(F_FULLFSYNC)`,
while a raw barrier-free `pwrite` on the same file is 3 µs at the median. So
`OsCache` is **not** the cheap barrier its name suggests, on macOS.

Any apparent `Full`-versus-`OsCache` difference elsewhere in the data is
run-to-run noise, not a durability effect, and this document therefore reports a
single durability configuration (`Full`) throughout
[Section 6.9](#69-throughput-and-latency-host). This is a **negative result about
the measurement platform**, not about the engine: the trade-off the option
exists to express cannot be demonstrated here.

The 8,005 µs per-page figure is the calibration constant used by the commit
latency model below.

### 6.9 Throughput and latency (host)

**Platform:** host, real engine, `FileFlash`, `Durability::Full`, 3 independent
freshly-formatted volumes per point.
**Command:** `cargo run --release -p slate-kv --example slate_throughput`.
**Data:** `throughput.csv` section `[summary_by_b_commit]`.

Geometry: 8 MiB capacity, 256 B pages, 4 KiB blocks, `Profile::Pi`,
`auto_b` off, 2,048 index keys, 100 B values, 1,000 distinct keys, 2,000 puts
and 2,000 gets, 3 reps. Commit-bearing puts are identified by `acked_seq()`
advancing, not by timing.

**Absolute values characterise this host's filesystem barrier, not any device.**
Only the shape versus $B$ and the commit/non-commit structure transfer.

| $B$ | Put (ops/s) | CV | Commit put (ms) | Other put (µs) | Ratio | Get (ops/s) | Get p50 (µs) |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 30.6 | 3.3% | 32.7 | — | — | 162,135 | 5.2 |
| 2 | 47.7 | 9.3% | 42.1 | 14.9 | 2,824 | 228,529 | 3.8 |
| 4 | 85.1 | 2.1% | 47.0 | 10.9 | 4,324 | 197,040 | 4.5 |
| 8 | 116.2 | 5.1% | 68.9 | 8.8 | 7,830 | 230,222 | 4.4 |
| 16 | 128.0 | 4.0% | 125.0 | 8.4 | 14,951 | 171,268 | 6.1 |
| 27 | 151.7 | 14.1% | 180.6 | 4.9 | 37,211 | 289,901 | 3.5 |
| 32 | 136.2 | 5.6% | 237.1 | 6.9 | 34,588 | 214,564 | 5.0 |
| 64 | 147.8 | 10.1% | 439.3 | 5.2 | 84,611 | 438,535 | 2.8 |
| 128 | 183.0 | 12.0% | 735.2 | 5.8 | 126,911 | 153,697 | 6.1 |

Throughput rises $5.98\times$ across the sweep, from 30.6 to 182.97 puts per
second, with $4.18\times$ of that reached by $B = 16$. Run-to-run variation is
small at the low end (CV 3.3% at $B = 1$) and grows at the high end (up to 14.1%
at $B = 27$), where each measurement spans fewer commits. The sweep is not
monotone above $B = 16$ — 151.7 at $B = 27$, 136.2 at $B = 32$, 147.8 at
$B = 64$ — and given the CVs those points should be read as a plateau rather
than as a ranking.

Reads are unaffected by the batch dial, as they should be: the index is resident,
and get throughput stays between 153,697 and 438,535 operations per second with
median latency between 2.8 and 6.1 µs.

**Latency is bimodal and a mean would misrepresent it.** One put in $B$ carries
the commit — it programs the marker pages and waits on the durability barrier
— and every other put appends to a RAM buffer. At $B = 128$ the commit-bearing
put takes 735.2 ms and the others take 5.8 µs, a ratio of 126,911. Batching does
not make the work cheaper; it concentrates it. For an application with a
real-time obligation this is the number that matters: raising $B$ improves
average throughput while making the worst-case single operation dramatically
worse.

**Commit latency is fully accounted for by page count.** Predicting
$\lceil 157B/256 \rceil + 3$ pages at the measured 8.005 ms per-page barrier
cost:

| $B$ | Predicted pages | Predicted (ms) | Measured (ms) | Measured / predicted |
|---:|---:|---:|---:|---:|
| 1 | 4 | 32.02 | 32.71 | 1.022 |
| 2 | 5 | 40.03 | 42.15 | 1.053 |
| 4 | 6 | 48.03 | 46.96 | 0.978 |
| 8 | 8 | 64.04 | 68.90 | 1.076 |
| 16 | 13 | 104.07 | 125.02 | 1.201 |
| 27 | 20 | 160.10 | 180.61 | 1.128 |
| 32 | 23 | 184.12 | 237.12 | 1.288 |
| 64 | 43 | 344.22 | 439.27 | 1.276 |
| 128 | 82 | 656.41 | 735.19 | 1.120 |

The ratio stays within $0.978\times$ to $1.288\times$ across the whole sweep, so
the engine adds no measurable cost of its own beyond the pages it writes.

![Performance](proposal/figures/fig3_performance.png)

### 6.10 Energy model and the optimal batch size

**Platform:** `SimFlash` in-RAM simulator for the flash traffic; joules from a
parameterised model. **Command:**
`cargo run --release -p slate-kv-sim --bin slate_energy_batch`. **Data:**
`energy_batch.csv`.

**Energy here is an estimate, not a measurement.** Flash traffic (bytes
programmed, erases, wakes, commits) is measured; the joules are those counts
multiplied through `slate_kv_sim::power::PowerModel`, which labels its own output
`ESTIMATED`. No power meter and no board were attached. Model constants, read
from `PowerModel::default()` at runtime: 200 nJ per byte, 5,000 µJ per erase
block, 1,000 µJ per wake, 1,024 CPU nJ per cycle (Q10), 24 AEAD cycles per byte.

Two byte accountings are reported because they answer different questions.
`report_bytes` is what `power::report()` sums (user + GC + parity + checkpoint)
and **omits `marker_bytes`** — `power::Stats` has no such field. `full_bytes`
is `SimFlash`'s `bytes_programmed`, the ground truth of what was actually
programmed, markers included. The omitted term is a $1/B$ term, so it matters
most at small $B$.

Geometry: 8 MiB `SimFlash`, 16 B values, 1,000 distinct keys, 4,000 operations,
deterministic, 1 rep.

| $B$ | Commits | `full_bytes` | `report_bytes` | $E_{full}$ (µJ/op) | $E_{report}$ (µJ/op) |
|---:|---:|---:|---:|---:|---:|
| 1 | 4000 | 4,096,000 | 1,568,000 | 1229.4 | 1087.8 |
| 2 | 2000 | 2,048,000 | 1,056,000 | 614.7 | 559.1 |
| 4 | 1000 | 1,280,000 | 800,000 | 321.7 | 294.8 |
| 8 | 500 | 768,000 | 672,000 | 168.0 | 162.6 |
| 16 | 250 | 512,000 | 608,000 | 91.2 | 96.5 |
| 27 | 148 | 416,768 | 581,888 | 60.4 | 69.7 |
| 32 | 125 | 384,000 | 576,000 | 52.8 | 63.5 |
| 64 | 62 | 317,440 | 559,872 | 33.5 | 47.2 |
| 128 | 31 | 293,632 | 551,936 | 24.4 | 39.0 |

Modelled energy per operation falls $50.4\times$ from $B = 1$ to $B = 128$ on the
ground-truth accounting.

Fitting $E(B) = A/B + P$ by ordinary least squares:

| Accounting | $A$ (µJ/commit) | $P$ (nJ/op) | $R^2$ |
|---|---:|---:|---:|
| `full_includes_markers` | 1,209.35 | 14,952.3 | 0.999905 |
| `report_omits_markers` | 1,056.77 | 30,717.7 | 0.999997 |

If uncommitted records are also charged a holding cost $c$ per
operation-second — the energy of keeping a record alive but not yet durable —
total power becomes convex in $B$ and admits a closed-form optimum of the
economic-order-quantity form,

$$B^\star = \sqrt{\frac{2 \lambda A}{c}}$$

for arrival rate $\lambda$. At $\lambda = 10$ operations per second, on the
ground-truth accounting:

| $c$ (nJ/op·s) | $B^\star$ closed form | $B^\star$ integer | Empirical argmin | Rel. error | Excess power at $B^\star$ |
|---:|---:|---:|---:|---:|---:|
| 3,000,000 | 2.84 | 2 | 3 | 5.35% | 0.00% |
| 1,000,000 | 4.92 | 4 | 5 | 1.64% | 0.00% |
| 300,000 | 8.98 | 8 | 9 | 0.23% | 0.00% |
| 100,000 | 15.55 | 15 | 15 | 3.68% | 1.63% |
| 30,000 | 28.39 | 28 | 30 | 5.35% | 0.89% |
| 10,000 | 49.18 | 49 | 45 | 9.29% | 1.51% |
| 3,000 | 89.79 | 89 | 90 | 0.23% | 0.00% |

The closed form is within 0.2% to 9.3% of the empirical minimum in batch size
across three decades of holding cost, and the resulting power penalty never
exceeds 1.6%. It is accurate enough to configure a device without sweeping it.
The byte counts underlying this are measured; the joules are not.

### 6.11 Index behaviour (in RAM)

**Platform:** pure in-RAM computation on the real `Index` type — no flash, no
simulator, no device. Byte counts are `u32`-slot arithmetic and so are identical
on 32-bit targets; load-factor, probe and collision counts are
hardware-independent.
**Command:** `cargo run --release -p slate-kv-core --example slate_index`.
**Data:** `index_ram.csv`, 8 RNG seeds $\times$ 7 table sizes $\times$ 2 key
families = 112 rows.

Each row fills a table to its first insertion failure, then refills a fresh
table to $\lfloor 0.95 \cdot \text{capacity} \rfloor$ and measures every stored
key plus 200,000 absent keys.

**Probe count is a constant, not a bound.** `probes_mean` $=$ `probes_max`
$= 16$ on all 112 rows, because `candidates()` scans both buckets and the whole
stash unconditionally. Lookup cost in the index is therefore independent of load
factor — mean equals worst case.

**Occupancy.** Mean load factor at first insertion failure is 0.9858 pooled
(0.9773 for the mixed family, 0.9943 for sequential, min 0.9694, max 1.0078 —
above 1.0 because the stash holds 8 entries beyond the arena). At the design
point $\alpha = 0.95$ the table costs 4.21 arena bytes per key across every
configuration (4.2105 to 4.2140).

**Fingerprint collisions are where the key family matters, and the two families
are not interchangeable.** `fingerprint()` is the top byte of FNV-1a, whose high
bits are mixed only through the multiply chain. The `mixed` family passes the
ordinal through a SplitMix64 bijection first and is the uniform population the
$2b \cdot 2^{-f}$ bound assumes. The `sequential` family is
`tag | seed | little-endian ordinal` — which is what an application writing
`sensor_%06d` keys actually produces — and leaves the high ordinal bytes
constant.

| `n_buckets` | Mixed mean | Mixed max | Sequential mean | Sequential max |
|---:|---:|---:|---:|---:|
| 256 | 0.02944 | 0.02997 | 0.02943 | 0.03064 |
| 512 | 0.02935 | 0.03016 | 0.02944 | 0.03077 |
| 1,024 | 0.02936 | 0.02957 | 0.03042 | 0.03270 |
| 2,048 | 0.02933 | 0.02992 | 0.02951 | 0.03734 |
| 4,096 | 0.02919 | 0.02990 | 0.03145 | 0.05563 |
| 8,192 | 0.02922 | 0.02950 | 0.02915 | 0.09337 |
| 16,384 | 0.02943 | 0.03018 | 0.03921 | 0.17685 |

Against the theoretical bound $2b \cdot 2^{-f} = 0.03125$:

- **Mixed keys:** mean 0.02933, max 0.03018, and the highest Wilson 95% upper
  bound over all rows is 0.03094 — comfortably below the bound, and consistent
  with the bound recomputed from the measured fingerprint histograms (0.02933 to
  0.02973).
- **Sequential keys:** up to **0.17685** at `n_buckets` $= 16384$ (seed 13),
  which is $5.66\times$ the bound, with a Wilson interval of
  $[0.17518, 0.17852]$ that excludes it decisively.

The degradation is size-dependent: sequential keys track the bound up to
`n_buckets` $= 2048$ and diverge above it (max 0.0556 at 4,096, 0.0934 at 8,192,
0.1768 at 16,384). The shipped configuration (`n_buckets` $= 2048$) is in the
benign regime, with a sequential maximum of 0.0373. **This is a real deployment
consequence** — a collision costs a wasted flash read and an AEAD open — and it
degrades exactly for the most natural embedded key-naming scheme. Recorded in
[Section 7.5](#75-standing-gaps), with a probed remedy and its caveats in
[Section 6.11.1](#6111-a-probe-of-the-remedy-modelled-not-the-engine).

#### 6.11.1 A probe of the remedy (modelled, not the engine)

**Platform:** host, pure in-RAM computation. **Command:**
`cargo run --release -p slate-kv-core --example slate_fp_remedy`. **Data:**
`fp_remedy.csv`, 4 table sizes $\times$ 3 seeds $\times$ 2 schemes = 24 rows,
50,000 absent lookups each, store filled to
$\lfloor 0.95 \cdot n_{buckets} \cdot 4 \rfloor$.

`scheme=shipped` is `index::fingerprint` (the top byte of raw FNV-1a);
`scheme=finalized` passes the hash through a SplitMix64 avalanche finalizer
first. Both arms use the same sequential keys and the same
`index::bucket1`/`index::alt_bucket` arithmetic, so the fingerprint function is
the only difference.

| `n_buckets` | Shipped mean | Shipped max | Shipped max / bound | Finalized mean | Finalized max | Finalized max / bound |
|---:|---:|---:|---:|---:|---:|---:|
| 256 | 0.03015 | 0.03100 | 0.99 | 0.02867 | 0.02946 | 0.94 |
| 1,024 | 0.03610 | 0.04742 | 1.52 | 0.02893 | 0.02938 | 0.94 |
| 4,096 | 0.01432 | 0.01470 | 0.47 | 0.02864 | 0.02914 | 0.93 |
| 16,384 | 0.01327 | 0.01370 | 0.44 | 0.02763 | 0.02866 | 0.92 |

The finalizer does what it is meant to do: it makes the rate **uniform and
bounded**. Across all 12 finalized cells the rate lies in
$[0.02636, 0.02946]$, every cell is under the bound (maximum 0.94 of it), and
there are zero violations. The distinct stored fingerprints saturate at 255 of
256 possible values in every finalized cell, against as few as 202 for the
shipped function --- which is the mechanism: the shipped function does not reach
most of the fingerprint space for these keys.

**Two cautions, both load-bearing.**

First, **this is a model of the remedy, not a measurement of a modified engine.**
`Index` has no hook to substitute the fingerprint function, so the harness
reimplements candidate selection over the public `index::` surface. A real fix
must be re-measured through `slate_index` against the actual `Index` type.

Second, **this harness's shipped arm does not reproduce the spike that motivated
the remedy.** `index_ram.csv` reports sequential-key rates rising with table size
to 0.17685 at `n_buckets` $= 16384$
([Section 6.11](#611-index-behaviour-in-ram)); this harness reports the shipped
scheme *falling* with table size, to 0.0133 at the same size --- below the bound.
Its worst shipped cell is 0.04742 at `n_buckets` $= 1024$ (1.52 of the bound,
with a Wilson interval of $[0.04559, 0.04932]$ that excludes it), and only 2 of
12 shipped cells exceed the bound at all. The two harnesses differ in key
construction detail, in store occupancy, and in absent-lookup count, and only one
of them drives the real `Index`.

The honest reading is therefore narrower than "the finalizer fixes the spike": the
finalizer demonstrably removes the *key-family sensitivity* of the fingerprint
distribution, in a model, and it does so at every table size. Whether it removes
the 0.17685 spike specifically is **not established by this file**, because that
spike does not appear in it. Closing that gap is part of the work item in
[Section 8.5](#85-strengthen-the-fingerprint-against-sequential-keys).

### 6.12 Mount cost (host)

**Platform:** host, `FileFlash`. Page and read *counts* are device-model exact
(256 B pages, 4 KiB blocks); the milliseconds are host-specific and include OS
page-cache effects.
**Command:** `cargo run --release -p slate-kv --example slate_recovery`.
**Data:** `recovery.csv`.

Geometry: 16 MiB capacity — the ceiling, since `mount` rejects
capacity $> 2^{24}$ and `Db::open` would turn that rejection into "format a new
volume", silently making every reopen replay nothing — 256 B pages, 4 KiB
blocks, `data_base` 540,672, `MAX_CKPT_LEN` 262,276, `CKPT_SLOTS` 2,
`THETA` 16,384. Workload: `b_commit` $= 8$, 1,024 keys (16,384 index slots),
12-byte keys, 64-byte values, 512 distinct keys, 5 reopens per point.

Two experiments are needed, because one sweep would confound the variables.
Confound guards: `tail_intact = 1` asserts that the realised `replay_from`
equals the log head at seal time *and* that `records_replayed` equals the
requested tail, so no unplanned `THETA` seal moved the checkpoint;
`had_checkpoint = 1` asserts mount loaded a checkpoint rather than reformatting.
All 14 rows satisfy both.

**Experiment 1 — sweep the replay tail, hold total volume fixed** at 2,000 base
records:

| Tail requested | Replayed | Ckpt read pages | Replay read pages | Mount read pages | Mount p50 (ms) | µs/record |
|---:|---:|---:|---:|---:|---:|---:|
| 0 | 0 | 260 | 1 | 261 | 0.43 | — |
| 10 | 10 | 260 | 125 | 385 | 0.50 | 50.27 |
| 50 | 50 | 260 | 625 | 885 | 1.26 | 25.21 |
| 100 | 100 | 260 | 1,251 | 1,511 | 1.62 | 16.23 |
| 500 | 500 | 260 | 6,251 | 6,511 | 2.97 | 5.93 |
| 1000 | 1000 | 260 | 12,257 | 12,517 | 10.97 | 10.97 |
| 4000 | 4000 | 260 | 48,257 | 48,517 | 19.96 | 4.99 |
| 8000 | 8000 | 260 | 96,257 | 96,517 | 36.62 | 4.58 |

Least squares over the 7 nonzero-tail rows:

$$\texttt{replay\_read\_pages} = 12.0242 \cdot \texttt{records\_replayed} + 110.4, \qquad R^2 = 0.999993$$

$$\texttt{mount\_read\_pages} = 12.0242 \cdot \texttt{records\_replayed} + 370.4, \qquad R^2 = 0.999993$$

$$\texttt{mount\_ms\_median} = 4.4183\,\mu\text{s} \cdot \texttt{records\_replayed} + 1.94\,\text{ms}, \qquad R^2 = 0.975436$$

Linear in the tail, as specified.

**Experiment 2 — hold the replay tail fixed at 200 records, sweep total
volume.** This is the experiment that tests the $O(\Theta)$ claim: if
checkpointing did not work, cost here would grow with volume.

| Base records | Log bytes | Replayed | Key verify calls | Mount read pages | Mount p50 (ms) |
|---:|---:|---:|---:|---:|---:|
| 0 | 204,800 | 200 | 1 | 2,263 | 2.48 |
| 100 | 227,584 | 200 | 6 | 2,273 | 3.33 |
| 1,000 | 428,800 | 200 | 200 | 2,761 | 2.03 |
| 5,000 | 1,324,800 | 200 | 200 | 2,761 | 4.56 |
| 20,000 | 4,684,800 | 200 | 200 | 3,017 | 1.54 |
| 40,000 | 9,164,800 | 200 | 200 | 3,017 | 3.99 |

Log bytes grow $44.8\times$ (204,800 to 9,164,800) while mount read pages grow
only $1.333\times$ (2,263 to 3,017) and are **identical** at 3,017 for 20,000 and
40,000 base records, i.e. saturated. `scan_bytes` is constant at 204,800 on every
row. Median mount time spans 1.536 to 4.561 ms with no monotone trend in volume
(Spearman $\rho = 0.143$). **The $O(\Theta)$ claim holds.**

The residual growth is fully explained and bounded. `ckpt_read_pages` steps from
260 to 516 once `base_records` exceeds `THETA`, because by then an automatic seal
has filled the second checkpoint slot with a full-size checkpoint instead of the
tiny genesis one; mount reads and AEAD-verifies *every* populated slot, and that
term is bounded by `CKPT_SLOTS` $= 2$. `key_verify_calls` — the full-key AEAD
verifications replay issues to resolve fingerprint collisions — grows with the
number of *live keys*, not the log volume, and saturates at one per replayed
record once all 512 distinct keys are live (visible as 1, 6, 200, 200, 200, 200
down the column).

**Timing caveat:** the median is over 5 reopens of the *same* on-disk volume, so
the first reopen is cold-cache and `mount_ms_max` is usually that one. A device
booting from cold flash is closer to max than to median. Wall-clock is not
resolvable below about 0.4 ms here, which is why the zero-tail row is dominated
by the fixed term. Mount on real ESP32-C3 or Raspberry Pi hardware was **not**
measured.

![Index and recovery](proposal/figures/fig4_index_recovery.png)

### 6.13 Garbage-collection model (modelled, not the engine)

**Platform:** a pure in-process GC model on the host — **no flash backend, and
not the SLATE engine.** **Command:**
`cargo run --release -p slate-kv-sim --bin wa_study_paper`. **Data:**
`wa_study.csv` (240 rows), `wa_study_matched.csv` (1,680 rows),
`wa_study_original.csv` (the original harness, verbatim, for comparison),
`wa_study_findings.json`.

The model omits RS(12,8) parity, commit markers, checkpoints and encryption. Its
absolute values therefore understate the engine by at least the $1.5\times$
parity floor. It is presented as a model of the **garbage-collection term
alone**, and MUST NOT be read as engine write amplification. The corresponding
engine measurement is [Section 6.7](#67-write-amplification-host).

Geometry: fixed capacity of 64 reference segments, 64 records per segment
(4,096 records), `n_keys` $=$ round($u_{target} \cdot 4096$), 100,000 operations
after the initial fill, `wa_steady` measured over the second half only. Sweep:
$u_{target} \in \{0.5, \dots, 0.9\}$ $\times$ Zipf $s \in \{0, 0.6, 0.9, 1.2\}$
$\times$ 4 GC arms $\times$ 3 seeds.

Single-head arm, mean over seeds, against the classical bound
$\mathrm{WA} \le 1/(1-u)$:

| $u$ | $s=0.0$ | $s=0.6$ | $s=0.9$ | $s=1.2$ | Bound $1/(1-u)$ | $s=0$ with parity floor |
|---:|---:|---:|---:|---:|---:|---:|
| 0.5 | 1.283 | 1.389 | 1.493 | 1.544 | 2.000 | 1.925 |
| 0.6 | 1.535 | 1.656 | 1.748 | 1.774 | 2.501 | 2.303 |
| 0.7 | 2.008 | 2.153 | 2.297 | 2.504 | 3.333 | 3.012 |
| 0.8 | 3.071 | 3.233 | 3.312 | 3.448 | 5.001 | 4.606 |
| 0.9 | 7.351 | 7.426 | 7.226 | 5.521 | 9.990 | 11.026 |

In the corrected harness the bound holds at **every one of the 240 cells**, with
at least 22.5% headroom at the worst cell, and there are **zero** degenerate rows
(no GC starvation, no progress-guard hits). Realised utilisation matches its
target to within 0.0001 at the reference capacity. Seed noise is under 1%
everywhere (max CV 0.864%, median 0.214%), and WA measured over the second half
of 100,000 operations is converged (max 0.784% change from 200k to 400k ops).

**Hot/cold segregation pays off only under skew.** Comparing the two-head arm
against the single-head arm *at matched capacity*, and against the arm given two
extra segments (which reproduces the original harness's configuration):

| $u$ | $s$ | Single | Hot/cold (matched) | Δ | Hot/cold (+2 segs) | Δ |
|---:|---:|---:|---:|---:|---:|---:|
| 0.5 | 0.0 | 1.283 | 1.284 | +0.04% | 1.255 | -2.23% |
| 0.5 | 0.6 | 1.389 | 1.375 | -1.00% | 1.344 | -3.21% |
| 0.5 | 0.9 | 1.493 | 1.435 | -3.89% | 1.402 | -6.10% |
| 0.5 | 1.2 | 1.544 | 1.481 | -4.08% | 1.440 | -6.75% |
| 0.6 | 0.0 | 1.535 | 1.543 | +0.53% | 1.482 | -3.45% |
| 0.6 | 0.6 | 1.656 | 1.651 | -0.26% | 1.586 | -4.20% |
| 0.6 | 0.9 | 1.748 | 1.711 | -2.08% | 1.647 | -5.77% |
| 0.6 | 1.2 | 1.774 | 1.723 | -2.88% | 1.672 | -5.74% |
| 0.7 | 0.0 | 2.008 | 2.019 | +0.55% | 1.886 | -6.09% |
| 0.7 | 0.6 | 2.153 | 2.139 | -0.64% | 1.995 | -7.33% |
| 0.7 | 0.9 | 2.297 | 2.206 | -3.98% | 2.060 | -10.32% |
| 0.7 | 1.2 | 2.504 | 2.043 | -18.41% | 1.971 | -21.27% |
| 0.8 | 0.0 | 3.071 | 3.095 | +0.80% | 2.720 | -11.42% |
| 0.8 | 0.6 | 3.233 | 3.228 | -0.14% | 2.840 | -12.15% |
| 0.8 | 0.9 | 3.312 | 3.243 | -2.10% | 2.900 | -12.46% |
| 0.8 | 1.2 | 3.448 | 2.597 | -24.67% | 2.412 | -30.05% |
| 0.9 | 0.0 | 7.351 | 7.651 | +4.09% | 5.304 | -27.85% |
| 0.9 | 0.6 | 7.426 | 7.699 | +3.66% | 5.424 | -26.97% |
| 0.9 | 0.9 | 7.226 | 6.719 | -7.03% | 5.015 | -30.59% |
| 0.9 | 1.2 | 5.521 | 4.625 | -16.23% | 3.587 | -35.02% |

At $s = 0$ the matched-capacity two-head arm is *worse* by up to 4.09%: two open
heads consume an extra reserve segment for no separation benefit. Under heavy
skew it wins substantially, by 16% to 25% at $s = 1.2$. Given the sub-1% seed
noise, the differences above 4% are real and the $s = 0$ near-tie is within
noise. The honest statement is that **age separation is real but requires
workload skew**; the large uniform-workload win the original harness reported was
extra capacity.

Two defects in the original harness, and the capacity artefact behind its
apparent bound violations, are in
[Section 7.2](#72-defect-2-two-defects-in-the-write-amplification-study).

### 6.14 Asynchrony

Every engine algorithm is written once as an `async fn` over `AsyncFlash`, with
the blocking API generated as a projection: each synchronous method is a one-line
body driving the corresponding future to completion on a busy-poll executor
([Section 5.3](#53-slate-kv-core-the-engine)). This subsection measures what that
construction does and does not deliver.

#### 6.14.1 Future sizes

**Platform:** host, release profile, `core::mem::size_of_val` on the future
actually returned by each async method, monomorphised over
`SimFlash`/`SimCounter`/`CryptoSealer` at ESP32 geometry.
**Command:** `cargo run -q --release -p slate-kv-sim --example slate_async_future_size`.
**Data:** `async_future_size.csv`.

A heapless async design lives or dies on future sizes, since each future is
materialised in the caller's stack frame. The 2,048 B figure below is the design
document's stated bound, applied here as an **external check**: the constant
`MAX_FUTURE_BYTES` does not exist anywhere in the source tree and no
compile-time assertion enforces it
([Section 7.3](#73-defect-3-unimplemented-claims-in-the-async-design-document)).

| Operation | Future bytes | Under 2,048 B |
|---|---:|:---:|
| `Slate::get_into_async` | 360 | yes |
| `Slate::index_update_offset_async` | 520 | yes |
| `Slate::index_remove_key_async` | 512 | yes |
| `Slate::append_cold_async` | 1,712 | yes |
| `Slate::append_cold_tombstone_async` | 1,696 | yes |
| `Slate::commit_async` | 1,656 | yes |
| `Slate::seal_epoch_now_async` | 520 | yes |
| `Slate::compact_async` | 1,624 | yes |
| `gc::compact_one_async` | 1,592 | yes |
| `Log::commit_async` | 1,304 | yes |
| `segment::encode_parity` | 1,424 | yes |
| `epoch::seal_epoch_async` | 496 | yes |
| `epoch::mount_async` | 616 | yes |
| `counterfactual::stack_local_1280B_across_await` | 1,328 | yes |
| `counterfactual::borrowed_1280B_buffer` | 80 | yes |
| `task::YieldNow` | 1 | yes |
| `struct::ScratchWorkspace` | 5,720 | **no** |
| `struct::Slate<SimFlash,SimCounter,CryptoSealer>` | 9,720 | **no** |
| `struct::Slate<BlockingFlash<SimFlash>,BlockingCounter<SimCounter>,CryptoSealer>` | 9,720 | **no** |
| `struct::SimFlash` | 224 | yes |
| `struct::CryptoSealer` | 168 | yes |
| `struct::EngineState` | 104 | yes |

All **13** engine futures fit, the largest being `Slate::append_cold_async` at
1,712 B. The rows marked `struct::` are not futures and are not expected to fit:
they are the RAM the workspace-hoisting technique **moved** rather than removed.

That distinction is the substantive finding. Hoisting a 1,280 B buffer into a
shared workspace and borrowing it across an await point shrinks the future from
1,328 B to 80 B — a $16.6\times$ reduction — but `ScratchWorkspace` is itself
5,720 B and lives inside `Slate` (which is 9,720 B), where
[Section 4.4](#44-ram-working-set) counts it. **Stack pressure is genuinely
reduced; the RAM total is not.** Hoisting MUST NOT be presented as free.

#### 6.14.2 Yield spans

**Platform:** host with a *simulated* flash-latency model, not a device.
**Command:** `cargo run -q --release -p slate-kv-sim --example slate_async_yield`.
**Data:** `async_yield.csv`.

Latency model, W25Q-class SPI NOR at 40 MHz, datasheet typicals: read 100 µs per
256 B page, program 500 µs per page, erase 45,000 µs per 4 KiB block
(datasheet *max* is 400 ms), counter increment 500 µs. A span is the simulated
time between consecutive `Poll::Pending` returns; spans $=$ yield points $+ 1$.

A note on why a custom adapter was needed: `SimFlash`'s own `read_lat_ms`,
`prog_lat_ms` and `erase_lat_ms` fields are `u64` **milliseconds** and are read
by no code path anywhere in the workspace (only their declaration and
initialisation to zero exist). A 100 µs page read is inexpressible with them, so
this harness wraps `SimFlash` in a microsecond latency accumulator.

Geometry: 2 MiB region, 8,192-key index, `b_commit` $= 8$, except the
`recover::recover` sweep, which used 16 MiB so a large tail fits.

| Path | Yield points | Spans | Total sim (ms) | Mean span (ms) | Max span (ms) | Erases in longest | Max span excl. one erase (ms) |
|---|---:|---:|---:|---:|---:|---:|---:|
| `Slate::commit_async[8 records]` | 0 | 1 | 12.5 | 12.5 | 12.5 | 0 | 12.5 |
| `Slate::seal_epoch_now_async[8192-slot index]` | 138 | 139 | 470.0 | 3.4 | 45.0 | 1 | 0.0 |
| `gc::compact_one_async[GC_YIELD_EVERY_RECORDS=8]` | 32 | 33 | 725.9 | 22.0 | 53.1 | 1 | 8.1 |
| `recover::recover[BLOCKING Flash trait; 128 records replayed]` | 0 | 1 | 72.1 | 72.1 | 72.1 | 0 | 72.1 |
| `recover::recover[BLOCKING Flash trait; 256 records replayed]` | 0 | 1 | 144.1 | 144.1 | 144.1 | 0 | 144.1 |
| `recover::recover[BLOCKING Flash trait; 512 records replayed]` | 0 | 1 | 288.1 | 288.1 | 288.1 | 0 | 288.1 |
| `epoch::mount_async[checkpoint load only]` | 2 | 3 | 13.1 | 4.4 | 13.0 | 0 | 13.0 |
| `recover::recover[BLOCKING Flash trait; 1024 records replayed]` | 0 | 1 | 576.1 | 576.1 | 576.1 | 0 | 576.1 |
| `recover::recover[BLOCKING Flash trait; 2048 records replayed]` | 0 | 1 | 1152.1 | 1152.1 | 1152.1 | 0 | 1152.1 |
| `recover::recover[BLOCKING Flash trait; 4096 records replayed]` | 0 | 1 | 2304.1 | 2304.1 | 2304.1 | 0 | 2304.1 |
| `recover::recover[BLOCKING Flash trait; 8192 records replayed]` | 0 | 1 | 4608.1 | 4608.1 | 4608.1 | 0 | 4608.1 |

Compaction bounds its longest uninterruptible span at 53.1 ms regardless of how
much work it does, and epoch sealing at 45.0 ms — both effectively at the 50 ms
figure the design document names, and both reducing to 8.1 ms and 0.0 ms
respectively once the single indivisible erase is excluded, which is the form the
document's own amendment states the criterion in.

**Mount-time replay has no yield point at all.** `recover.rs` is on the blocking
`Flash` trait, contains no `await`, and is therefore one span by construction
(`yield_points = 0`). Replaying 8,192 records is a single uninterruptible span of
**4,608.1 ms** — about $92\times$ the 50 ms figure. On a device that must
service a radio during startup this is the system's binding real-time
constraint, and it is the one path where the asynchronous design does not apply.
The span scales exactly linearly with the tail (72.1 ms at 128 records,
144.1 at 256, 288.1 at 512, 576.1 at 1,024, 1,152.1 at 2,048, 2,304.1 at 4,096).

A tail of `THETA` $= 16{,}384$ records could not be measured: the engine's
`reserve_space_async` seals a checkpoint before the tail gets that long, so mount
loads the *newer* checkpoint and the replayable tail collapses to zero. The
largest directly measurable tail is 8,192 records.

**Yield cadence sweep.** `GC_YIELD_EVERY_RECORDS` was swept by editing the
constant, rebuilding, re-running and restoring the file to its committed value of
8 — it is a plain `pub const u16` with no runtime override.

| Cadence | Yield points | Flash ops | Mean span (ms) | Max span (ms) | Max excl. one erase (ms) |
|---:|---:|---:|---:|---:|---:|
| 1 | 174 | 851 | 4.15 | 45.2 | 0.2 |
| 2 | 93 | 851 | 7.72 | 45.2 | 0.2 |
| 4 | 52 | 851 | 13.70 | 53.1 | 8.1 |
| 8 | 32 | 851 | 22.00 | 53.1 | 8.1 |
| 16 | 22 | 851 | 31.56 | 53.1 | 8.1 |
| 32 | 17 | 851 | 40.33 | 53.1 | 8.1 |
| 64 | 14 | 851 | 48.39 | 89.6 | 44.6 |
| 128 | 13 | 851 | 51.85 | 141.3 | 141.3 |

`flash_ops_total` is invariant at 851 across the entire sweep. This is direct
confirmation of the design's binding rule: **changing yield cadence MUST NOT
change the flash operation sequence.** The shipped cadence of 8 sits at the knee
— cadences 1 through 32 all bound the max span at 53.1 ms, while 64 and 128
degrade it to 89.6 ms and 141.3 ms.

#### 6.14.3 The cost of the blocking projection

**Platform:** host; CPU time from macOS `getrusage(RUSAGE_SELF)` user+sys.
**Command:** `cargo run -q --release -p slate-kv-sim --example slate_async_blocking_cost`.
**Data:** `async_blocking_cost.csv`.

The executors are hand-rolled poll loops. `embassy-executor` was **not** used: it
is a dependency of `targets/esp32` only, is not a host dependency of any
workspace crate, and its ESP32 time driver does not build for
`aarch64-apple-darwin`. Arm A is verbatim `slate_kv_core::task::block_on`
(busy poll with `core::hint::spin_loop()`). Statistics are medians, with arms
interleaved rep-by-rep so frequency drift hits them equally.

Two flash models, answering different questions. `BlockingFlash<SimFlash>` is the
production sync façade, in which no future ever suspends for an I/O reason —
this is the configuration the "zero-cost" claim is about. `DeadlineFlash`
suspends for a real 45,000 µs on erase (W25Q `tSE` typical), standing in for a
DMA- or interrupt-backed QSPI driver.

| Flash model | Executor | Reps | Wall p50 (ms) | CPU p50 (ms) | CPU/wall | Pending polls |
|---|---|---:|---:|---:|---:|---:|
| task::block_on (busy poll |  spin_loop hint) | 41 | 4.824 | 4.825 | 100.0% | 0 |
| bare poll loop (native async |  no spin hint) | 41 | 4.813 | 4.812 | 100.0% | 0 |
| task::block_on (busy poll |  spin_loop hint) | 5 | 1215.565 | 1213.843 | 99.9% | 52,621,624 |
| parking executor (100 us park per Pending) | 5 | 300 | 36.086 | 2.800 | 9438.0% | 236 |

**When the driver never suspends, the projection is genuinely free:** 4.813 ms
versus 4.824 ms, a 0.23% difference, which is noise.

**When the driver does suspend, it is not free.** Both executors face the
identical 45 ms erase; only what they do during it differs. Wall-clock is
comparable (1,215.6 ms busy-poll versus 1,270.8 ms parking, a 4.54% penalty) but
CPU time is 1,213.8 ms versus 36.1 ms — 99.9% CPU occupancy against 2.8%, a
97.03% reduction. The busy-poll façade spins through **52,621,624**
`Poll::Pending` returns waiting for flash, against 9,438 for the parking arm.

The 4.54% wall penalty is the parking arm's 100 µs granularity overshooting the
45 ms deadline, not engine cost; an executor woken by a completion interrupt
would not pay it.

A throughput benchmark would score these two configurations as equivalent. On a
battery-powered node they are not remotely equivalent. **The correct statement of
the claim is that the blocking projection is zero-cost *when the flash driver
never suspends*,** and the design document should be amended to say so.

#### 6.14.4 Façade structure

**Data:** `async_facade.json` (source reads, `grep`, `cargo tree`, `llvm-nm`,
probe-crate compile matrix).

Confirmed by source read and programmatic check: exactly one `impl` block for
the core type at `slate.rs:66`; all 8 `Slate` blocking wrappers and the `Log`
wrapper have one-line bodies; `#![forbid(unsafe_code)]` present in both
`slate-kv-core` and `slate-kv-hal` with zero `unsafe` outside doc comments;
`cargo tree -p slate-kv-core --all-features -e normal` contains no `embassy-*`
crate, so the core is executor-agnostic; `size_of::<YieldNow>() = 1` byte.

One feature-gating finding is specification-relevant. **The `async` cargo feature
is inert.** `grep` for `feature = "async"` over `slate-kv-hal/src/` and
`slate-kv-core/src/` returns zero hits; only `feature = "embedded-storage-async"`
gates anything (the `storage_async` module). A probe crate depending on both with
`default-features = false` compiles a function generic over `AsyncFlash`, one
over `AsyncMonotonicCounter`, a `BlockingFlash` construction and
`task::yield_now()` — exit 0. So `AsyncFlash`, `AsyncMonotonicCounter`,
`BlockingFlash`, `BlockingCounter`, `YieldNow` and `block_on` are compiled
**unconditionally**, and the bare-metal build pays for them regardless of the
feature. This document therefore MUST NOT be read as claiming the async surface
is opt-in. The `blocking` feature, by contrast, does gate: the same probe fails
with `error[E0599]: no method named commit found` when it is off.

![Async](proposal/figures/fig5_async.png)

### 6.15 Device run (ESP32-C3)

**Platform:** real ESP32-C3 silicon, real NOR flash, `embassy_demo` firmware,
counters reported by the firmware over the serial link. Nine checkpoint reports
over 8,112 records at `b_commit` $= 8$.
**Data:** `device_c3.csv`, `device_c3_analysis.json`.

Geometry as read from the device: 2 MiB region, `data_base` 540,672, usable
1,556,480 B, 256 B pages, 4 KiB blocks, 31 segments, 74 B records (30 B of key
plus value), checkpoint 33,024 B.

| Records | Epoch | Hot head | Segs free | Segs sealed | `user_bytes` | `marker_bytes` | `ckpt_bytes` | Erases | WA |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1,000 | 2 | 732,672 | 27 | 2 | 74,000 | 64,000 | 33,024 | 21 | 2.7435 |
| 2,000 | 3 | 924,672 | 24 | 5 | 148,000 | 128,000 | 66,048 | 42 | 2.7435 |
| 3,000 | 4 | 1,116,672 | 21 | 8 | 222,000 | 192,000 | 99,072 | 63 | 2.7435 |
| 4,000 | 5 | 1,308,672 | 18 | 11 | 296,000 | 256,000 | 132,096 | 84 | 2.7435 |
| 5,000 | 6 | 1,500,672 | 15 | 14 | 370,000 | 320,000 | 165,120 | 105 | 2.7435 |
| 6,000 | 7 | 1,692,672 | 12 | 17 | 444,000 | 384,000 | 198,144 | 126 | 2.7435 |
| 7,000 | 8 | 1,884,672 | 9 | 20 | 518,000 | 448,000 | 231,168 | 147 | 2.7435 |
| 8,000 | 10 | 2,076,672 | 29 | 0 | 592,000 | 512,000 | 297,216 | 441 | 2.7993 |
| 8,112 | 10 | 2,096,640 | 29 | 0 | 600,288 | 518,656 | 297,216 | 441 | 2.7911 |

**The instrument is self-consistent.** The device's reported write amplification
of 2.7435 recomputes from its own byte buckets to 2.74357, a discrepancy of
$7 \times 10^{-5}$.

**The erase trajectory decomposes exactly.** A checkpoint is 33,024 B, which is
9 erase blocks; a segment is 12 blocks. The device erases 21 blocks per thousand
records in steady state — exactly $9 + 12$, one checkpoint plus one reclaimed
segment. The reclamation burst near the end, 294 erases, resolves as
$2 \times 9 + 23 \times 12 = 294$: two checkpoints plus 23 segments freed at
once. Garbage collection demonstrably works.

**Exhaustion is predictable.** An analytic model of flash consumption predicts
exhaustion at 8,107 records; the device halted at 8,112, an error of 0.06%.

**A gap in the byte accounting.** Reconciling the model against the counters
exposes a discrepancy the metrics do not attribute. The metrics count 203.024 B
of writes per record, but the log region actually receives 192 B per record of
*log* traffic against 170 B the metrics attribute to the log — a difference of
**22 B per record of page padding** belonging to no bucket
([Section 5.7](#57-metrics-bucket-definitions)). Consequently reported
amplification of 2.7436 understates the padding-inclusive figure of 3.0409 by
**10.8%**. Neither number is wrong for its own definition, but a device-lifetime
calculation MUST use the second.

**Reclaimed space is not reusable.** The device halted with `FlashFull` at 8,112
records while **29 of its 31 segments were free and erased**, with the hot head
512 B short of the region end. This is the engine's most consequential
deviation from this specification; see [Section 7.5](#75-standing-gaps).

![Device trajectory](proposal/figures/fig6_device_trajectory.png)

![Fault tolerance](proposal/figures/fig1_fault_tolerance.png)

---

## 7. Known deviations from this specification

Measuring a system is a good way to discover that some of what it claims is not
so. Four defects surfaced during the measurement work: one in the engine, two in
the project's own instrumentation and study, and one reporting defect. They are
recorded here in full, together with the standing gaps between what this
specification describes and what the code at `970324f` does.

### 7.1 Defect 1: `user_bytes` double-counted (FIXED)

**Status:** fixed. **Data:** `user_bytes_bug.json`.

While measuring throughput, `user_bytes` was found to be counted **twice** on
the std and sim wrapper paths. `Slate::append_hot`
(`crates/slate-kv-core/src/slate.rs:244`) counts
`REC_OVERHEAD + klen + vlen` so that `no_std` targets measure amplification at
all; five wrapper call sites counted the same record again —
`slate-kv/src/db.rs` `put` plus its two delete paths, and
`slate-kv-sim/src/sim_db.rs` `put` plus its two delete paths.

The measured ratio was exactly **2.0000**, confirmed independently on both flash
backends.

The effect inflates the denominator while leaving overhead untouched, so
**reported amplification was too low**:

$$\mathrm{WA}_{reported} = \frac{\mathrm{WA}_{true} + 1}{2}, \qquad \mathrm{WA}_{true} = 2\,\mathrm{WA}_{reported} - 1$$

| Batch | Reported before fix | Correct after fix |
|---:|---:|---:|
| $B = 1$ | 3.6539 | **6.3078** |
| $B = 128$ | 1.0541 | **1.1082** |

**Fix:** the five duplicate `add_user_bytes` calls were removed, leaving a single
call site in the core append path. Verification after the fix: `cargo fmt` clean,
`clippy -D warnings` clean, 63 tests passed and 0 failed. Every affected figure
was regenerated; `wa_buckets.csv` carries a note recording the regeneration, and
[Section 6.7](#67-write-amplification-host) reports the corrected values.

**The device log was never affected.** The firmware calls `append_hot` directly
and never goes through a wrapper, so its `user_bytes` (74 B per record) and its
amplification (2.7435) were always single-counted. The device figures in
[Section 6.15](#615-device-run-esp32-c3) required no correction.

This defect is reported partly *because* the corrected numbers are worse than the
originals. Instrumentation that flatters the engine is more dangerous than an
engine that is simply slow, and a metrics definition is normative
([Section 5.7](#57-metrics-bucket-definitions)) precisely so that this class of
error is detectable.

### 7.2 Defect 2: two defects in the write-amplification study

**Status:** original harness defective; corrected harness ships alongside it.
**Data:** `wa_study_findings.json`, `wa_study_original.csv`, `wa_study.csv`,
`wa_study_matched.csv`.

**(a) The "measured" utilisation column was a closed form.** The original
harness reported a `meas_u` that was in fact
$n_{keys}/\text{cap\_records}$ — a closed form of the target — and never read
simulator state. The evidence is that it is byte-identical across all four skew
levels at every $u$ (0.496, 0.590, 0.694, 0.781, 0.893 for the single-head arm).
Worse, device capacity varied with $u$ as a side effect, from 63 segments at
$u = 0.5$ down to 35 at $u = 0.9$, so the $u$ axis also swept physical capacity
by $1.8\times$ — confounding the very sweep the study existed to perform. The
corrected harness reads realised utilisation out of simulator state and matches
its target to within 0.0001 at fixed capacity.

**(b) The bound assertion excluded every violating case.** The study's
self-check was gated on `if u <= 0.8`, and it printed "passed all assertions".
Above that gate, **3 of 40 cells violate** $\mathrm{WA} \le 1/(1-u)$, by up to
**18.45%**:

| $u$ | $s$ | Arm | WA | Realised $u$ | Bound | Excess |
|---:|---:|---|---:|---:|---:|---:|
| 0.9 | 0.6 | single | 11.07 | 0.893 | 9.346 | +18.45% |
| 0.9 | 0.0 | single | 10.85 | 0.893 | 9.346 | +16.10% |
| 0.9 | 0.9 | single | 9.77 | 0.893 | 9.346 | +4.54% |

**The root cause is a small-capacity artefact, not a failure of the bound.** The
2–3 segment GC reserve is a fixed overhead: at 35 segments it is 5.7% of
capacity, so a gross $u = 0.90$ is really a net $u = 0.954$ on
allocator-reachable capacity, and $1/(1-0.954) = 22.0$ — against which the
measured 12.2 is well inside. A capacity sweep at $u = 0.9$, single head, makes
this explicit:

| Capacity (segs) | WA | Bound | Violations | Note |
|---:|---:|---:|---:|---|
| 16 | — | — | — | degenerate: GC starves |
| 24 | 236.3 | — | — | degenerate |
| 35 | 12.2 | 10.0 | 9 of 12 | the original harness's operating point |
| 48 | 8.22 | 10.01 | 0 | |
| 64 | 6.88 | 9.99 | 0 | |
| 96 | 5.98 | 10.01 | 0 | |
| 128 | 5.61 | 10.0 | 0 | |

**Normative consequence for anyone quoting the bound:** state it as
$\mathrm{WA} \le 1/(1 - u_{net})$ where $u_{net}$ excludes the GC reserve, and
state the capacity regime. As a statement about gross $u$ it is falsified at
small capacity (up to $+2269\%$ at 24 segments, $+21.5\%$ at 35) and holds only
for capacity $\ge 48$ segments.

**(c) A capacity confound in the policy comparison.** The hot/cold arm was given
two extra segments. Re-run at matched capacity the apparent win at zero skew
inverts: the original reported hot/cold 5.20 against single 10.85 ($-52\%$) at
$s = 0$, $u = 0.9$; at equal gross capacity it is 7.65 against 7.35, i.e.\ $+4.1\%$
**worse**. The sign flips. Under heavy skew the effect survives strongly
($-16\%$ to $-25\%$ at $s = 1.2$). See
[Section 6.13](#613-garbage-collection-model-modelled-not-the-engine).

**(d) The model omits parity entirely.** With RS(12,8), parity imposes a
multiplicative floor of $1.5\times$ that the model never charges, so its absolute
values understate the engine by that factor even where its trends are right.

The study reported passing all its assertions. That report was not evidence.

### 7.3 Defect 3: unimplemented claims in the async design document

**Status:** the code is as measured; the design document (`docs/design/018`)
asserts machinery that does not exist. **Data:** `async_facade.json`,
`async_future_size.csv`, `async_yield.csv`.

Constants the document tabulates as enforcement mechanisms:

| Claimed | Reality at `970324f` |
|---|---|
| `MAX_FUTURE_BYTES = 2048`, "compile-time assert" | **Does not exist.** `grep` returns zero hits. The `const _` assertion block and the `crate::probe` module it references do not exist. The 2,048 B bound is met by construction today ([Section 6.14.1](#6141-future-sizes)) and nothing would catch a regression. |
| `MAX_YIELD_SPAN_MS = 50`, "documentation constant + test bound" | **Does not exist.** Zero hits repository-wide. No test asserts any yield-span bound. |
| `RECOVER_YIELD_EVERY_PAGES = 32` | **Exists** at `config.rs:46` but is **referenced nowhere** — only its declaration. It is dead code, because `recover.rs` is still on the blocking `Flash` trait. |
| `SlateSync<'a, F, C, S>` newtype | **Does not exist.** The projection was done instead as `cfg`-gated inherent methods on `Slate` itself. |
| `segment::write_parity` | No such symbol. The function is `segment::encode_parity`, and **it has no callers**. |
| Debug assertion pairing `block_on` with `BlockingFlash` | Does not exist. |

Test suites the document names as the feature's acceptance criteria:

| Claimed suite | Reality |
|---|---|
| Op-sequence equivalence over 1,000 seeds — the binding rule's **enforcement mechanism** | No such test exists. No async-versus-blocking trace comparison anywhere in `crates/*/tests/`. |
| Drop-equivalence / cancellation-safety at every await point | No such test exists. |
| Yield-span bound test | No such test exists. |
| Future-size regression check | No such test exists. |
| Executor portability smoke test under Embassy on the QEMU harness | Not present. |
| Interrupt-latency / heartbeat-jitter measurement on hardware — called by the document "the headline claim of the feature", to be measured and not asserted | **Not present, and the instrument does not exist.** |

**The demonstration binary does not use its executor.** `embassy_demo` is not an
Embassy binary: its `embassy_executor::Spawner` and `embassy_time` imports are
commented out (`embassy_demo.rs:4-5`), both of its declared
`#[embassy_executor::task]` functions — `heartbeat_task` and
`jitter_logger_task` — sit inside a block comment (lines 32–66), its entry point
is a plain synchronous function, and every engine call goes through the busy-poll
bridge. Link-level confirmation: `llvm-nm` on the ELF shows **zero** symbols from
`embassy_executor` or `embassy_time`; every "embassy" substring in the symbol
table is the crate-name suffix. The async engine *is* linked (monomorphised
`block_on`, `commit_async`, `index_update_offset_async` over
`BlockingFlash<EspFlash>`), so the binary is a genuine compile-and-run smoke test
of the async core through the blocking bridge — but it is not an async firmware.

**Consequence, stated plainly: no asynchronous-versus-blocking comparison on
hardware is possible at this revision, because no asynchronous firmware exists to
measure.** Nothing in this document may be read as such a comparison. In
particular, the $9{,}368$ B `.text` difference between `embassy_demo` and
`kv_demo` is **not** "what async costs": `kv_demo` carries a UART command
interpreter and a self-check harness that `embassy_demo` does not, and both link
the identical async core.

**The `async` feature is inert** — see
[Section 6.14.4](#6144-façade-structure). The async HAL surface is compiled
unconditionally, so the design document's statement that the traits are "gated
behind `feature = "async"`, default OFF" does not describe the code, and the
bare-metal build pays for the surface regardless.

One historical suspicion was checked and **cleared**: a vestigial `if false`
block around `gc.rs:295-297` that would have made a yield unreachable. It was
introduced by `fc024e9` and removed by `a0932e0`; `git grep 'if false'` at HEAD
returns zero hits. Both GC yield sites (`gc.rs:348`, `gc.rs:498`) are live and
both were exercised in the yield measurement.

### 7.4 Defect 4: genesis security-mode misreport

**Status:** reporting defect only. **Data:** `tamper.json`, section
`security_mode`.

The `SecurityMode` reported immediately after a genesis format does not reflect
the counter kind. `mount`'s `FormatError` branch constructs `EngineState` with a
hardcoded `BestEffortRollback`, so a `Hardware` counter is misreported as
`BestEffortRollback` until the first remount, where it correctly becomes `Full`.
Measured: `sim_counter_kind = Hardware`,
`sim_counter_path_on_genesis_format = BestEffortRollback`,
`sim_counter_path_on_remount = Full`.

This is a **labelling defect, not a safety one**: a freshly formatted volume has
no prior epoch to roll back to, so no rollback check is skipped. But any figure
or log line quoting `security_mode` MUST state whether it was read on genesis or
on remount.

### 7.5 Standing gaps

Beyond the four defects, five gaps bound what this specification's requirements
actually deliver at `970324f`.

**(a) Reclaimed space is not reusable — the most consequential gap.** A device
halts with `FlashFull` while 29 of 31 segments are free and erased
([Section 6.15](#615-device-run-esp32-c3)). GC works; the hot log head simply
cannot wrap into the freed space. It advanced to 512 B short of the region end
and stopped. Two format-level causes, both pre-existing:

1. records straddle segment boundaries, because the writer runs the head straight
   through parity blocks and segment boundaries
   ([Section 2.4](#24-record-encoding));
2. recovery replays forward from the checkpoint head to the first erased page
   ([Section 3.7](#37-tail-replay)), so a wrapped head would make the tail
   unreplayable.

The ordering mechanism a circular log needs already exists — a segment-header
scan sorted by `seg_seq`, `recover::scan_segment_headers` — but **nothing ever
writes those headers** ([Section 2.9](#29-segment-header)). Closing this gap is a
format change requiring reformat. The test that asserts the property is the
suite's one ignored test.

**(b) The RAM budget is exceeded.** 83,092 B resident and 87,764 B at the mount
peak, against a documented 64 KiB — over by 26.8% and 33.9%
([Section 4.4](#44-ram-working-set)). The dominant unexpected term is the
checkpoint buffer at 32,900 B, which must hold the entire serialized index.
Either the budget or the shipped configuration must change; the largest
configuration that fits is `n_buckets` $= 1024$ at 49.14 KiB and roughly 3,891
keys, and that configuration is not reachable through `Db::open` today.

**(c) Mount replay does not yield.** A single uninterruptible span of 4,608 ms at
8,192 records, about $92\times$ the design document's 50 ms figure
([Section 6.14.2](#6142-yield-spans)). This is the system's binding real-time
constraint on a device that must service a radio during startup.

**(d) Sequential keys defeat the fingerprint.** Collision rates up to
$5.66\times$ the theoretical bound (0.17685 against 0.03125) for the most natural
embedded key-naming scheme ([Section 6.11](#611-index-behaviour-in-ram)). The
shipped table size is in the benign regime; larger tables are not. A finalizer
remedy has been probed in a model and removes the key-family sensitivity, but it
does not reproduce this spike, so the gap remains open
([Section 6.11.1](#6111-a-probe-of-the-remedy-modelled-not-the-engine)).

**(e) Reported amplification is not physical amplification.** Page padding, 22 B
per record on the device workload, is attributed to no metrics bucket, so
reported WA understates the padding-inclusive figure by 10.8%
([Section 6.15](#615-device-run-esp32-c3)). A lifetime calculation must use the
padding-inclusive number.

Two further limitations bound the *evidence*, not the engine.

**Host latencies are not device latencies.** Every absolute timing in
[Sections 6.9](#69-throughput-and-latency-host) and
[6.12](#612-mount-cost-host) characterises a laptop filesystem barrier. Only
[Section 6.15](#615-device-run-esp32-c3) reports silicon, and it reports byte and
erase counts rather than latencies. A latency characterisation on hardware
remains to be done.

**The durability trade-off cannot be shown on this platform.** `Full` and
`OsCache` are indistinguishable on macOS
([Section 6.8](#68-host-flash-barrier-calibration)), and on Linux they are
identical by construction — which was not measured, as no Linux host was
available.

### 7.6 Disagreements between data files and earlier prose

The data files are authoritative. Where an earlier prose description of this work
disagrees with a data file, the following corrections apply.

| Quantity | Earlier prose | Data file | Correct value |
|---|---|---|---|
| GC share of bytes written | "under 0.4% across the entire sweep" | `wa_buckets.csv` | Up to **0.95%** (at $B = 4$); it is 0.37% at $B = 1$ and 0% at most points. The claim holds only at $B = 1$. |
| Amplification reduction $B{=}1 \to B{=}128$ | "$5.7\times$" | `wa_buckets.csv` | $6.3078/1.1082 = \mathbf{5.69\times}$ (rounds to $5.7\times$; stated here to the precision the data supports). |
| WA at $B = 2$ | 3.62 | `wa_buckets.csv` | **3.6160** |
| WA at $B = 4$ | 2.38 | `wa_buckets.csv` | **2.3773** |
| Throughput at $B = 32$ | 151.9 ops/s | `throughput.csv` | **136.23** ops/s. 151.7 is the $B = 27$ row; the sweep is not monotone above $B = 16$. |
| Get throughput range | "154,000 to 229,000 ops/s" | `throughput.csv` | **153,697 to 438,535** ops/s over the `Full` rows. |
| Get median latency | "near 5 µs throughout" | `throughput.csv` | **2.8 to 6.1 µs**, and not monotone. |
| Commit-put CV at $B = 128$ | "12.0%" | `throughput.csv` | 12.02% at $B = 128$, but the **maximum** CV is 14.09% at $B = 27$. |
| Commit latency model agreement | "$0.98$ to $1.29\times$" | `throughput.csv` | Confirmed: $0.978\times$ to $1.288\times$. |
| Energy fixed cost $A$ | 1209 µJ/commit, $R^2 = 0.9999$ | `energy_batch.csv` | **1,209.35** µJ/commit, $R^2 = 0.999905$, on the `full_includes_markers` accounting. The payload term is **14,952 nJ/op**, i.e. 15.0 µJ/op. |
| $B^\star$ example | "28.4 against an empirical minimum at 30, 5.4% error, 0.9% excess power" | `energy_batch.csv` `[bstar]` | At $c = 30{,}000$ nJ/op·s: $B^\star = \mathbf{28.394}$, empirical argmin **30**, relative error **5.35%**, excess power **0.89%**. Values agree; the grid point is $c = 30{,}000$ nJ/op·s, not "0.03 mJ" loosely stated. |
| Index load factor at first failure | "0.986" | `index_ram.csv` | **0.9858** pooled over both key families; 0.9773 for `mixed` alone and 0.9943 for `sequential`. |
| Fingerprint bits | "$f = 9$" | `index_ram.csv`, `config.rs` | $f = \mathbf{8}$ (`FP_BITS = 8`). The bound $2b \cdot 2^{-f} = 0.03125$ is correct; the exponent quoted was not. |
| Sequential collision worst case | "0.177, nearly $6\times$ the bound"; elsewhere "$5.7\times$" | `index_ram.csv` | **0.176845**, which is $\mathbf{5.66\times}$ the 0.03125 bound. |
| Sequential-key collision trend with table size | (a single source was assumed) | `index_ram.csv` **vs** `fp_remedy.csv` | **The two harnesses disagree.** `index_ram.csv` has the shipped fingerprint degrading *with* table size (0.0295 at 2,048 rising to 0.17685 at 16,384); `fp_remedy.csv` has it *improving* with table size (0.04742 at 1,024 falling to 0.0133 at 16,384). Both are host in-RAM computations; only `index_ram.csv` drives the real `Index` type. Reported as an open discrepancy, not resolved — see [Section 6.11.1](#6111-a-probe-of-the-remedy-modelled-not-the-engine). |
| Harness file names | data-file headers say `paper_*` | repository at time of writing | The measurement harnesses were **renamed `paper_* ` to `slate_*`** after the data files were generated, so every `command:` line inside the data-file headers names a path that no longer exists. The commands in [Section 6.2](#62-reproduction-commands) use the current names. Values in the data files are unaffected. |
| Mixed-key collision rate | "0.0293, 95% upper bound 0.0301" | `index_ram.csv` | Mean **0.02933**; the highest Wilson 95% upper bound over all mixed rows is **0.03094**. |
| Mount replay slope | "12.02 pages per record" | `recovery.csv` | Confirmed: **12.0242**, $R^2 = 0.999993$. |
| Volume-sweep growth | "$44.8\times$ volume, $1.33\times$ reads" | `recovery.csv` | Confirmed: 204,800 to 9,164,800 B, 2,263 to 3,017 pages. |
| Mount replay span at 8,192 records | "4.6 s" | `async_yield.csv` | **4,608.1 ms**. |
| GC longest yield span | "53 ms" | `async_yield.csv` | **53.1 ms**; 8.1 ms excluding one erase. |
| Epoch-seal yield span | "45 ms" | `async_yield.csv` | **45.0 ms**; 0.0 ms excluding one erase. |
| Busy-poll spin count | "52.6 million" | `async_blocking_cost.csv` | **52,621,624**. |
| Blocking-façade CPU reduction | "97%" | `async_blocking_cost.csv` | **97.03%**, for a **4.54%** wall-clock penalty. |
| Zero-cost margin | "0.23%" | `async_blocking_cost.csv` | Confirmed: $-0.23\%$ wall, $-0.27\%$ CPU. |
| Device exhaustion error | "0.07%" | `device_c3_analysis.json` | $\lvert 8112 - 8107 \rvert / 8112 = \mathbf{0.06\%}$. |
| Device padding understatement | "11%" | `device_c3_analysis.json` | $3.0409/2.7436 - 1 = \mathbf{10.8\%}$. |
| Device metrics bytes per record | "170 B counted, 192 B actual" | `device_c3_analysis.json` | The log-region figure is 192 B actual against 170 B attributed to the log; total metrics-counted bytes per record are **203.024** B. Both framings appear; the 22 B padding gap is the invariant. |
| Firmware SLATE static buffers | "76,008 B in `.data`" | `firmware_size.csv` | Confirmed: $4108 + 4108 + 32780 + 35012 = 76{,}008$ B, identical across all three engine-linking binaries. |
| Resident RAM | "83,092 B / 81.1 KiB" | `ram_working_set.csv` | Confirmed as the **minimum required** total. The **as-built** `kv_demo` total is **85,204 B / 83.21 KiB**, using the 35,012 B `CKPT_BUF` instead of the 32,900 B minimum. |
| Segment count / `MAX_SEGMENTS` | "MAX_SEGS = 256" | `config.rs`, `gc.rs` | Both exist: `config::MAX_SEGS = 256` and `gc::MAX_SEGMENTS = 128`. The binding cap is **128**. |
| Erasure stripe records | "27 records, 2,017 bytes" | `erasure.csv` | Confirmed. |
| Tamper matrix disposition | "5 refused, 10 mounted" | `tamper.json` | Confirmed for the 15 non-control attacks: **5 refused** (2 `Rollback`, 3 `Tampered`), **10 mounted**, plus 3 pristine controls = 18 rows. |

---

## 8. Remaining work

In priority order, derived from the deviations of
[Section 7](#7-known-deviations-from-this-specification). Each item names what
must change and what it costs.

### 8.1 Close the space-reuse gap

**Why first:** it is what stands between the current engine and a device that
runs indefinitely ([Section 7.5](#75-standing-gaps)(a)). Everything else on this
list is an improvement; this one is a functional ceiling.

Three changes, all in concert:

1. write the segment header of [Section 2.9](#29-segment-header) at segment open,
   so `seg_seq` is materialised on flash;
2. roll the log head at the data/parity boundary rather than running it straight
   through, so records stop straddling segment boundaries;
3. order recovery by `scan_segment_headers` sorted on `seg_seq` rather than
   scanning linearly to the first erased page, so a wrapped head is replayable.

**Cost:** a format change requiring reformat of existing volumes. The reader side
(`recover::scan_segment_headers`) already exists.

**Done when:** `space_reuse_after_reclaim` passes without `#[ignore]`, and a
device run writes past the point at which the current firmware halts with 29 free
segments.

### 8.2 Reconcile the RAM budget

Three options, not mutually exclusive
([Section 7.5](#75-standing-gaps)(b)):

1. reduce the shipped index to `n_buckets` $= 1024$ (49.14 KiB resident, ~3,891
   keys) — and lift the `Db::open` floor of
   `max(n_keys, 2048)` so the configuration is reachable through the std API at
   all ([Section 4.4](#44-ram-working-set));
2. revise the documented 64 KiB budget to account for the checkpoint buffer, and
   state the new figure with the buffer named;
3. **stream the checkpoint** so the buffer need not hold the whole serialized
   index at once. This removes 32,900 B — the dominant term — and is the only
   option that keeps both the key capacity and the budget. It is also the most
   work: it changes the checkpoint write path, the AEAD framing (a streamed
   payload cannot be sealed as one buffer without an incremental construction),
   and the two-slot layout arithmetic.

**Done when:** the arithmetic of [Section 4.4](#44-ram-working-set) reproduces a
resident total at or under 65,536 B for the shipped configuration, cross-checked
against `llvm-nm` on the firmware.

### 8.3 Convert recovery to the async interface

Port `recover::recover` (and `record_key_eq`, `scan_segment_headers`) from
`Flash` to `AsyncFlash`, and honour `RECOVER_YIELD_EVERY_PAGES` in the scan loop.
This removes the system's longest uninterruptible span — 4,608 ms at 8,192
records — and makes an already-declared constant live rather than dead
([Sections 7.3](#73-defect-3-unimplemented-claims-in-the-async-design-document),
[7.5](#75-standing-gaps)(c)).

**Constraint:** the commit path MUST remain yield-free between the two marker
copies ([Section 3.2](#32-commit-and-the-acknowledgement-rule)). Converting
recovery does not license adding yield points inside commit.

**Done when:** `async_yield.csv` regenerated shows `yield_points > 0` and a
bounded `max_span_ms` for `recover::recover` at 8,192 records.

### 8.4 Build the async instrument

The async feature's headline claim — reduced interrupt latency and task jitter
under seal and GC load — has never been measured, and the binary that was to
carry the measurement has its Embassy tasks commented out
([Section 7.3](#73-defect-3-unimplemented-claims-in-the-async-design-document)).
Four pieces, in dependency order:

1. an **executor-based firmware binary** that actually links Embassy: uncomment
   the tasks, restore the imports, use an async entry point, and verify with
   `llvm-nm` that executor symbols are present;
2. the **op-sequence equivalence suite** the design document designates as the
   enforcement mechanism for its binding rule — run the same workload through
   the async and blocking façades and assert the flash operation sequences are
   identical, over many seeds. The cadence sweep of
   [Section 6.14.2](#6142-yield-spans) is evidence the rule holds for one
   workload; a test is what keeps it holding;
3. **cancellation-safety tests** at every await point: drop the future
   mid-operation and assert the on-flash state is one the recovery rules accept;
4. a **compile-time future-size assertion** — define `MAX_FUTURE_BYTES`, add the
   `const _` assertion, and thereby convert
   [Section 6.14.1](#6141-future-sizes) from an external check into an enforced
   bound.

Then measure interrupt latency and heartbeat jitter on hardware, which is the
claim the feature exists to make.

Also in this area, and cheap: either make the `async` cargo feature gate
something or remove it, since at present it gates nothing and the documentation
describing the async surface as opt-in is wrong
([Section 6.14.4](#6144-façade-structure)).

### 8.5 Strengthen the fingerprint against sequential keys

Mix the key more thoroughly before taking the fingerprint byte — a finalisation
step on the FNV-1a output, or a different hash — so that
`sensor_%06d`-style keys do not concentrate in a narrow fingerprint range
([Section 7.5](#75-standing-gaps)(d)). The target is the mixed-family rate
(0.0293) at every table size, rather than 0.1768 at `n_buckets` $= 16384$.

A SplitMix64 avalanche finalizer on the FNV-1a output has been probed and looks
sufficient: it holds the rate in $[0.02636, 0.02946]$ at every table size with
zero bound violations, against a shipped arm that varies by $3.7\times$ across the
same grid ([Section 6.11.1](#6111-a-probe-of-the-remedy-modelled-not-the-engine)).
Two things must happen before that is a result rather than an indication.

1. **Give `Index` a substitution point** for the fingerprint function, or measure
   the modified function in place. The probe reimplements candidate selection over
   the public `index::` surface because no hook exists, so it is a model.
2. **Reconcile the two harnesses.** The probe's shipped arm does not reproduce the
   0.17685 spike that `index_ram.csv` reports at `n_buckets` $= 16384$ — it reports
   0.0133 there. Until that spike is reproduced under the probe's conditions, or
   the difference in key construction and occupancy is explained, the remedy cannot
   be said to address it. Re-measure through `slate_index` against the real `Index`
   type and compare against the `mixed` family baseline.

**Note:** `fingerprint()` output is stored in index slots but is **not** part of
the on-flash format — the checkpoint stores slots, so changing the function
invalidates existing checkpoints but not the log. The compatibility cost is a
checkpoint format version bump, not a reformat.

### 8.6 Attribute page padding

Add the page-padding bytes to a metrics bucket so reported amplification matches
physical amplification ([Section 7.5](#75-standing-gaps)(e)). Either add a
`padding_bytes` field or fold padding into `user_bytes`; the former preserves the
existing bucket semantics and is preferable. On the device workload this is 22 B
per record, a 10.8% correction.

### 8.7 Characterise on hardware

Latency and energy on the ESP32-C3 and on a Raspberry Pi class device, so that
the *shapes* measured in [Sections 6.9](#69-throughput-and-latency-host) and
[6.10](#610-energy-model-and-the-optimal-batch-size) can be anchored to real
magnitudes. Two specific gaps:

- **Energy is modelled, not measured.** A power meter on a board would turn the
  $B^\star$ result from a model prediction into a measurement.
- **Mount timing on real flash.** The page counts transfer; the milliseconds do
  not.

Also worth doing while a Linux host is available: confirm that `Full` and
`OsCache` are identical there by construction, which is currently an unmeasured
claim ([Section 6.8](#68-host-flash-barrier-calibration)).

### 8.8 Smaller items

- **`repair::scrub` is a stub** returning `Ok(())`
  ([Section 5.3](#53-slate-kv-core-the-engine)). Either implement the scrub —
  read every segment, verify parity, reconstruct declared erasures — or remove
  the method rather than exposing one that silently succeeds.
- **`segment::encode_parity` has no callers** anywhere in the workspace. Parity
  is written elsewhere; either route the seal path through this function or
  delete it.
- **Fix the genesis security-mode misreport**
  ([Section 7.4](#74-defect-4-genesis-security-mode-misreport)): construct
  `EngineState` from `ctr.kind()` in the `FormatError` branch rather than
  hardcoding `BestEffortRollback`.
- **Consider whether the twin commit marker earns its cost.** It doubles the
  marker term — the dominant amplification term at small $B$
  ([Section 6.7](#67-write-amplification-host)) — and
  [Section 6.6](#66-at-rest-tampering-host-and-simulated) shows it does not
  protect against loss of the leading magic byte. Either make the scanner reach
  copy 2 when copy 1's magic is destroyed (which would make the redundancy do
  what its presence implies), or drop the twin and halve the marker cost.
- **Give `SimFlash`'s latency fields effect or remove them.** `read_lat_ms`,
  `prog_lat_ms` and `erase_lat_ms` are `u64` milliseconds read by no code path,
  and their millisecond granularity cannot express a 100 µs page read
  ([Section 6.14.2](#6142-yield-spans)). The yield harness had to wrap
  `SimFlash` in its own microsecond accumulator; that accumulator should become
  the simulator's own facility.

---

## 9. Appendix A: data file inventory

All paths relative to `docs/proposal/data/`. Twenty-seven files.

| File | Contents | Platform | Cited in |
|---|---|---|---|
| `provenance.json` | Revision, toolchain, host, gate results, line counts, firmware section sizes | host | [6.1](#61-provenance), [1.5](#15-crate-layout) |
| `testsuite.json` | Per-suite test counts: 63 passed, 0 failed, 1 ignored | host | [6.1](#61-provenance) |
| `crash_mc.json` | 20,000 crash trials, 5,000 rollback attempts, geometry, wall time | simulated | [6.3](#63-crash-injection-simulated), [6.4](#64-rollback-resistance-simulated) |
| `erasure.csv` | Exhaustive RS(12,8) enumeration, declared and undeclared modes, space overhead | pure computation | [6.5](#65-erasure-coding-pure-computation) |
| `tamper.json` | 18 at-rest attacks with per-attack rationale; security-mode findings | host + simulated | [6.6](#66-at-rest-tampering-host-and-simulated), [7.4](#74-defect-4-genesis-security-mode-misreport) |
| `wa_buckets.csv` | Byte buckets and WA against $B$, real engine | host | [6.7](#67-write-amplification-host) |
| `wa_study.csv` | Corrected GC model, 240 rows at fixed capacity | modelled | [6.13](#613-garbage-collection-model-modelled-not-the-engine) |
| `wa_study_matched.csv` | Capacity-sensitivity sweep, 1,680 rows | modelled | [7.2](#72-defect-2-two-defects-in-the-write-amplification-study) |
| `wa_study_original.csv` | Original harness verbatim, 40 rows | modelled | [7.2](#72-defect-2-two-defects-in-the-write-amplification-study) |
| `wa_study_convergence.csv` | Steady-state convergence check over `n_ops` | modelled | [6.13](#613-garbage-collection-model-modelled-not-the-engine) |
| `wa_study_findings.json` | Defect verdicts, capacity sweep, hot/cold decomposition, seed spread | modelled | [7.2](#72-defect-2-two-defects-in-the-write-amplification-study) |
| `wa_study.png` | GC model plot | modelled | — |
| `throughput.csv` | Sectioned: `[per_run]`, `[summary_by_b_commit]`, `[flash_barrier_calibration]` | host | [6.8](#68-host-flash-barrier-calibration), [6.9](#69-throughput-and-latency-host) |
| `energy_batch.csv` | Sectioned: `[sweep]`, `[user_bytes_double_count_check]`, `[fit]`, `[bstar]` | simulated traffic, modelled joules | [6.10](#610-energy-model-and-the-optimal-batch-size) |
| `index_ram.csv` | 112 rows: load factor, bytes/key, probes, collisions, stash, Wilson intervals | pure in-RAM | [6.11](#611-index-behaviour-in-ram) |
| `fp_remedy.csv` | Fingerprint finalizer probe: shipped vs SplitMix64-finalized, 24 rows | modelled (not the `Index` type) | [6.11.1](#6111-a-probe-of-the-remedy-modelled-not-the-engine) |
| `recovery.csv` | Tail sweep and volume sweep with confound guards and cost decomposition | host | [6.12](#612-mount-cost-host) |
| `ram_working_set.csv` | Three tables: per-term breakdown, totals, sweep over table size | static analysis | [4.4](#44-ram-working-set) |
| `firmware_size.csv` | Four binaries, section sizes, engine-linkage flag, per-buffer sizes | static analysis | [4.5](#45-firmware-static-footprint) |
| `async_future_size.csv` | 13 engine futures, 2 counterfactuals, 6 struct sizes | host | [6.14.1](#6141-future-sizes) |
| `async_yield.csv` | Per-path yield spans; cadence sweep | simulated latency | [6.14.2](#6142-yield-spans) |
| `async_yield_span.png` | Yield-span plot | simulated | — |
| `async_blocking_cost.csv` | Two flash models, two executors, wall and CPU medians, pending polls | host | [6.14.3](#6143-the-cost-of-the-blocking-projection) |
| `async_facade.json` | Façade verification, feature-gating probe, 12 unimplemented doc-018 claims, `embassy_demo` finding | static analysis | [6.14.4](#6144-façade-structure), [7.3](#73-defect-3-unimplemented-claims-in-the-async-design-document) |
| `device_c3.csv` | Nine checkpoint reports over 8,112 records | **device** | [6.15](#615-device-run-esp32-c3) |
| `device_c3_analysis.json` | Derived geometry, erase decomposition, exhaustion prediction, padding gap | **device** | [6.15](#615-device-run-esp32-c3) |
| `user_bytes_bug.json` | Root cause, ratio, affected files, fix, before/after values | host + simulated | [7.1](#71-defect-1-user_bytes-double-counted-fixed) |

Harnesses added to the working tree to produce these files, all committed
alongside:

| Path | Produces |
|---|---|
| `crates/slate-kv/examples/slate_wa_buckets.rs` | `wa_buckets.csv` |
| `crates/slate-kv/examples/slate_throughput.rs` | `throughput.csv` sections 1–2 |
| `crates/slate-kv/examples/slate_flash_calib.rs` | `throughput.csv` section 3 |
| `crates/slate-kv/examples/slate_recovery.rs` | `recovery.csv` |
| `crates/slate-kv-core/examples/slate_index.rs` | `index_ram.csv` |
| `crates/slate-kv-core/examples/slate_fp_remedy.rs` | `fp_remedy.csv` |
| `crates/slate-kv-sim/examples/rs_exhaustive.rs` | `erasure.csv` |
| `crates/slate-kv-sim/examples/tamper_matrix.rs` | `tamper.json` |
| `crates/slate-kv-sim/examples/probe_security_mode.rs` | `tamper.json` security-mode section |
| `crates/slate-kv-sim/examples/slate_async_future_size.rs` | `async_future_size.csv` |
| `crates/slate-kv-sim/examples/slate_async_yield.rs` | `async_yield.csv` |
| `crates/slate-kv-sim/examples/slate_async_blocking_cost.rs` | `async_blocking_cost.csv` |
| `crates/slate-kv-sim/src/bin/crash_mc.rs` | `crash_mc.json` |
| `crates/slate-kv-sim/src/bin/slate_energy_batch.rs` | `energy_batch.csv` |
| `crates/slate-kv-sim/src/bin/wa_study_paper.rs` | `wa_study.csv`, `wa_study_matched.csv` |
| `crates/slate-kv-sim/src/bin/wa_study.rs` | `wa_study_original.csv` (original harness) |

Figures referenced in this document live in `docs/proposal/figures/` as both
`.png` (used here) and `.pdf`.

---

## 10. Appendix B: reading this document alongside the formal paper

The formal companion paper states the durability, freshness and erasure
properties as theorems with proofs, over an abstract model of the flash device.
This specification is the other half: it says what the implementation at
`970324f` actually does, where the abstraction is not yet met, and which measured
evidence supports which claim.

The mapping is:

| Property | Formal statement | Implementation mechanism | Evidence |
|---|---|---|---|
| Prefix durability | Recovered state is a prefix of the acknowledged sequence | Commit marker + chain value + `seq_max` three-way test | [6.3](#63-crash-injection-simulated): 20,000 trials, 0 violations |
| Freshness / rollback resistance | A stale image is rejected at mount | Epoch bound to an external monotonic counter; boot rule | [6.4](#64-rollback-resistance-simulated): 5,000 of 5,000 rejected; [6.6](#66-at-rest-tampering-host-and-simulated) exercises both halves of the rule |
| Erasure tolerance | Up to `RS_M` declared block erasures per segment are recoverable | RS(12,8) Cauchy over $\mathrm{GF}(2^8)$, encoded at seal | [6.5](#65-erasure-coding-pure-computation): 1,586 patterns, 0 wrong bytes |
| Integrity under tampering | A modified record fails to open rather than decrypting to attacker-chosen plaintext | ChaCha20-Poly1305 with the header as associated data | [6.6](#66-at-rest-tampering-host-and-simulated): 18 attacks, 0 wrong values |
| Bounded mount cost | Mount is $O(1) + O(\Theta)$, not $O(\text{volume})$ | Checkpointed index; tail replay from `write_offset` | [6.12](#612-mount-cost-host): $44.8\times$ volume gives $1.33\times$ reads, then saturates |
| Bounded RAM | Working set is a compile-time constant | No heap in the core; borrowed arena | [4.4](#44-ram-working-set): exact, and **over the documented budget** |
| Bounded uninterruptible span | Every path yields within a bounded interval | `task::yield_now` at the cadence constants | [6.14.2](#6142-yield-spans): holds for GC and sealing, **fails for mount replay** |

Where the two documents differ in emphasis: the formal paper's model assumes the
flash driver honours the contract of
[Section 2.1](#21-requirements-on-the-flash-driver); this document states that
contract as a set of driver requirements and records, in
[Section 6.5](#65-erasure-coding-pure-computation), exactly what happens when the
declared-erasure part of it is violated.
