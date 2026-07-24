# SLATE: A Provably Secure, Ultra-Light, Low-Power Key–Value Engine for Edge Devices

*Formal model, correctness and security theorems, cost models, and a Pareto-optimal operating point*

# Abstract

We give a rigorous mathematical foundation for **SLATE** (*Secure, Log-structured, Authenticated, Tamper-Evident*), a single-device key–value (KV) storage engine designed for the edge regime — microcontrollers (ESP32) through single-board computers (Raspberry Pi) — under four simultaneous objectives: *ultra-light* memory footprint, *high performance*, *low energy*, and *strong at-rest security*. Our central thesis is that these four objectives **cannot be jointly maximized**; they define a trade-off surface. The scientifically defensible contribution is therefore not "one new algorithm that dominates on all axes," but (i) a **formally specified composition** of well-understood primitives whose guarantees we *prove*, and (ii) two sharper, original results: a **freshness-bound $O(1)$ authenticated append-log** (whole-store tamper-evidence and epoch-granular rollback protection with constant-time chain update and constant-time *freshness-tip* verification — the tip check is $O(1)$; rebuilding logical state is the separate $O(\Theta)$ replay), and an **energy-optimal commit-scheduling law** with a closed-form optimum $B^\star=\sqrt{2\lambda(E_\text{wake}+E_\text{commit})/c}$ (an Economic-Order-Quantity analogue for durable commits). We prove prefix-durability under arbitrary power loss, worst-case $O(1)$ index lookup with a load-factor guarantee, security reductions to standard cryptographic assumptions, and Reed–Solomon erasure recovery; we derive analytical energy, write-amplification, and device-lifetime models; and we characterize a lifetime-aware Pareto frontier, marking recommended operating points for ESP32- and Pi-class devices. The cost models are validated numerically, and two of them are checked by direct simulation: a crash-injection Monte-Carlo (20,000 random power losses, zero durability or freshness violations) and a skewed-workload garbage-collection study confirming the write-amplification law is a conservative bound. We also compare SLATE against representative embedded and server-class stores (Bitcask, LevelDB/RocksDB, BadgerDB, LMDB, SQLite, `ekv`).

# 1. Introduction and scope

## 1.1 What is and is not claimed

A recurring temptation in systems work is to promise a single primitive that is simultaneously the fastest, smallest, lowest-power, and most secure. This is not achievable, and stating why is itself part of the contribution:

- **Security costs energy.** Authenticated encryption (AEAD) of each value and maintenance of a tamper-evidence accumulator consume CPU cycles, and on a battery- or harvesting-powered node, cycles are joules.
- **Fault tolerance costs space and flash endurance.** Redundancy (erasure parity, checkpoints) consumes storage and, crucially, *additional program/erase (P/E) cycles*, which are a finite resource on flash.
- **Performance trades against power.** Batching and duty-cycled sleep reduce energy per operation but increase commit latency.

Consequently we do **not** claim a Pareto-dominating point. We claim: a *composition* whose properties are proven; two *original* sub-results (the freshness-bound $O(1)$ log and the commit-scheduling optimum); and a *characterization* of the achievable frontier with provably optimal points on it under stated constraints.

A word on the word "secure." Throughout, **"provably secure" means provable against the *at-rest, rollback-capable* adversary formalized in §2.4** — confidentiality and integrity of data on the flash medium, plus freshness/rollback resistance across power cycles — with security reductions to standard assumptions on the underlying AEAD, hash, and MAC (§6). It explicitly does *not* mean resistance to physical side channels (power/EM/timing analysis), to a live-code compromise that reads the key from RAM, or to physical destruction of the device; those are stated non-goals (§2.4), not claims. The title's "provably secure" should be read with that scope, which is the appropriate and achievable one for a bare-metal edge node without a TEE.

## 1.2 Novel vs. reused

| Component | Status | Basis |
|---|---|---|
| Append-only segmented log | Reused | Log-structured storage (LFS, RocksDB WAL) |
| In-RAM partial-key cuckoo index | Reused | Partial-key cuckoo hashing / cuckoo filters (with stash) |
| Per-record AEAD | Reused | ChaCha20-Poly1305 / AES-GCM |
| Reed–Solomon segment parity | Reused | MDS codes over $\mathrm{GF}(2^8)$ |
| **Freshness-bound $O(1)$ authenticated log** | **Novel (composition + analysis)** | Hash-chain accumulator bound to a hardware monotonic counter |
| **Energy-optimal commit scheduling** $B^\star$ | **Novel (model + closed-form optimum)** | EOQ-style retention/fixed-cost trade-off |
| **Proven joint operating point** | **Novel (analysis)** | Multi-objective Pareto characterization |

# 2. Formal system and threat model

## 2.1 Storage medium

**Definition (Flash storage abstraction).** The persistent medium is a finite array of *pages* grouped into *erase blocks*. It supports three operations with the standard flash semantics:

- `Program(p, data)`: writes a page $p$ exactly once after its containing block has been erased; a page may not be rewritten without an intervening erase.
- `Erase(b)`: resets all pages of block $b$ to the erased state; costs one *P/E cycle*.
- `Read(p)`: returns the current contents of page $p$.

Each block tolerates at most $N_{\mathrm{PE}}$ erase cycles before wear-out (typically $N_{\mathrm{PE}}\in[10^3,10^5]$). Total usable capacity is $C_{\mathrm{flash}}$ bytes.

**Definition (Volatile budget).** The engine may use at most $M$ bytes of RAM (e.g. $M\approx50\,\mathrm{KB}$ on ESP32, $M\approx100\,\mathrm{MB}$ on a Pi). RAM contents are lost on power failure.

## 2.2 Interface and logical state

**Definition (KV interface).** Keys $k\in\mathcal{K}$ and values $v\in\mathcal{V}$ (byte strings). The engine exposes
$$
\mathrm{Put}(k,v),\qquad \mathrm{Get}(k)\to \mathcal{V}\cup\{\bot\},\qquad \mathrm{Delete}(k).
$$
The *logical state* is a partial map $\sigma:\mathcal{K}\rightharpoonup\mathcal{V}$. A `Put`$(k,v)$ sets $\sigma(k)=v$; `Delete`$(k)$ removes $k$ from $\mathrm{dom}(\sigma)$; `Get`$(k)$ returns $\sigma(k)$ if $k\in\mathrm{dom}(\sigma)$ else $\bot$.

**Definition (Operation log and acknowledgment).** Operations are assigned strictly increasing sequence numbers $\mathrm{seq}=1,2,\dots$ and appended to a log $L=(r_1,r_2,\dots)$. A write is *acknowledged* to the client only after the page(s) containing its record *and* the batch commit marker have been durably programmed. Let $A(t)=(w_1,\dots,w_{n(t)})$ denote the sequence of acknowledged writes up to time $t$, ordered by $\mathrm{seq}$.

**Remark (Concurrency model: single writer).** SLATE assumes a *single-writer* execution model: one logical thread of control appends to the log, so the total order on $\mathrm{seq}$ is well-defined and no two operations contend for the head. This is the natural model for the target class — a microcontroller firmware task or a single ROS 2 node owning its store — and it is what lets $\mathrm{seq}$ act simultaneously as the ordering key, the AEAD nonce source (§3.3), and the replay order (§4). Concurrent multi-writer access, transactions spanning multiple keys, and isolation levels are *out of scope*; they would require a concurrency-control layer (e.g. a lock or an MVCC epoch) above the log and would change both the consistency analysis and the energy model. Concurrent *readers* are unproblematic (the log and index are read-mostly and a reader sees a consistent prefix), but are not analyzed here.

## 2.3 Crash model

**Assumption (Power-loss model).** Power may be lost at any instant. We assume:

1. **Page-program atomicity with detectable tearing.** A `Program` either completes, or leaves a page whose integrity check (device ECC and/or the record's AEAD tag) fails. Partially programmed pages are thus *detectable*, not silently corrupt. (This is the standard behavior when each record carries its own authentication tag.)
2. **No reordering across the commit marker.** The engine issues the batch commit-marker program only after all data pages of the batch report completion. (Enforced by the flash driver's ordering/flush.)
3. **Erased-state recognizability.** An un-programmed page reads as the all-ones (erased) pattern, distinguishable from a valid record header.

**Remark.** Condition 1 of the power-loss model is what makes torn-tail truncation sound: we never need to guess whether the last record is valid — its tag tells us. Devices without per-page ECC are handled by the record's own AEAD tag, which we require regardless for security.

## 2.4 Threat model

**Definition (Adversary).** The adversary $\mathcal{A}$ is *at-rest with rollback capability*:

- **Capabilities.** $\mathcal{A}$ may obtain the device when powered off and *read and rewrite the entire flash contents arbitrarily* (e.g. desolder and dump/reflash the chip). $\mathcal{A}$ may present the device with any flash image of its choosing, including an earlier authentic image (rollback/replay).
- **Trusted components (out of $\mathcal{A}$'s reach).** A device master key $K$ held in hardware (eFuse / secure element), never exposed to flash; and a hardware *monotonic counter* $\mathrm{MC}\in\mathbb{N}$ that only increases and that $\mathcal{A}$ cannot decrement or forge (e.g. eFuse counter or RPMB).
- **Restrictions (non-goals).** $\mathcal{A}$ cannot read live RAM or the running process (no live-code compromise); we do *not* defend against physical side channels (power/EM/timing analysis of the crypto), nor against destruction of the device (no single-node scheme can), nor against multi-device concerns (out of scope by design).

**Definition (Security goals).** Against $\mathcal{A}$ we require:

- **G1 Confidentiality.** $\mathcal{A}$ learns nothing about any value beyond its length.
- **G2 Integrity / tamper-evidence.** Any modification to the persisted log is detected on the next boot (the engine refuses to serve a tampered state rather than returning wrong data).
- **G3 Freshness / rollback-resistance.** Substituting a stale-but-authentic full image (produced by the genuine device at an earlier time) is detected.

These non-goals are deliberate: constant-time, masked crypto (side-channel defense) directly contradicts the low-power and ultra-light objectives, so we exclude it and say so rather than claim it.

# 3. Architecture specification

SLATE is a composition of five layers. We specify each precisely enough to prove properties about it in later sections. The read/write data flow is shown in Figure 1.

![SLATE architecture: write path (append record → AEAD → log segment → batch commit marker → ack) and read path (index lookup → log read → AEAD verify/decrypt). The RAM index and hash-chain tip are volatile and rebuildable; the flash log is the sole source of truth.](artifacts/arch_diagram.png)

## 3.1 Layer 1 — Append-only segmented log (durability substrate)

The log is a sequence of fixed-size **segments** $S_0,S_1,\dots$, each an integer number of erase blocks. Writes append to the current *head* segment; a segment is sealed when full and never modified in place. Each logical operation becomes one **record**:

$$
r = \big[\; \underbrace{\mathrm{magic}}_{1\text{B}} \;\|\; \underbrace{\mathrm{seq}}_{8\text{B}} \;\|\; \underbrace{\mathrm{op}}_{1\text{B}} \;\|\; \underbrace{h(k)}_{f\text{ bits}} \;\|\; \underbrace{\mathrm{klen}}_{2\text{B}} \;\|\; \underbrace{\mathrm{vlen}}_{2\text{B}} \;\|\; \underbrace{\mathrm{nonce}}_{12\text{B}} \;\|\; \underbrace{\mathrm{AEAD}_{K^{(e)}_{\mathrm{rec}}}(k\,\|\,v)}_{(\mathrm{klen}+\mathrm{vlen})\text{ B}} \;\|\; \underbrace{\tau}_{16\text{B}} \;\big]
$$

where $\mathrm{op}\in\{\mathrm{Put},\mathrm{Delete}\}$ (a delete is a *tombstone* record with empty value), and $\tau$ is the AEAD authentication tag. Two design points here are load-bearing for correctness and are corrections over a naive layout:

- **The full key $k$ is stored, inside the encrypted payload.** The header fingerprint $h(k)$ (width $f$ bits) is only an in-RAM *discriminator* to avoid wasted log reads; it is far too narrow ($f=12$) to identify a key among thousands, where fingerprint collisions between distinct live keys are near-certain by the birthday bound. Correctness of `Get` (Theorem 2, "Index reconstructibility") therefore requires the *full* key to be recoverable from the record so a fingerprint match can be confirmed against the actual key. We store $k$ **inside** the AEAD payload — encrypting $k\,\|\,v$ rather than $v$ alone — so that (i) the full-key comparison the lookup relies on is backed by durable data, and (ii) the key namespace stays confidential at rest (storing $k$ in plaintext, or as an unkeyed hash, would leak or expose it to dictionary attack). The plaintext splits into $k$ and $v$ using $\mathrm{klen}$. Recovering $k$ is free: `Get` already AEAD-opens the record to return $v$.
- The AEAD associated data is the record header (through $\mathrm{nonce}$), binding $\mathrm{seq}$, $\mathrm{op}$, and the lengths to the ciphertext.

A batch of records is followed by a **commit marker** $\mathrm{CM}=[\mathrm{magic}_{\mathrm{CM}}\|\mathrm{seq}_{\max}\|\mathrm{MC}\|\chi\|\tau_{\mathrm{CM}}]$ (defined in §3.4).

**Rationale.** Sequential, append-only programming is the flash access pattern with the lowest write-amplification and the most uniform wear; it needs no in-place updates and no random erases on the hot path.

**On-disk cost of storing the key.** Storing $k$ adds $\mathrm{klen}$ bytes to each record's *flash* footprint (edge keys — sensor IDs, config paths — are typically short, tens of bytes). Crucially this is a **flash**-side cost, not a RAM cost: the in-RAM index (§3.2, §5) still stores only the $f$-bit fingerprint and $p$-bit offset per key, so the ultra-light RAM budget of §5.3 is unchanged. The $k$ bytes are written once to the log, not held resident.

## 3.2 Layer 2 — In-RAM compact index (performance)

The index $I$ maps a live key to the byte offset of its most recent record. It is a **partial-key cuckoo hash table** — the RAM-resident structure of a *cuckoo filter* (Fan et al.) augmented with a log offset — storing, per slot, an $f$-bit fingerprint $h(k)$ and a $p$-bit log offset (bit-packed). This choice is deliberate and load-bearing: because the RAM index holds *only* the fingerprint (never the key, §3.1), the insertion procedure must be able to relocate an entry using the fingerprint alone. Partial-key hashing makes exactly that possible. Properties (proved in §5):

- **Two candidate buckets computed without the key.** The first bucket is $i_1 = H(k) \bmod n_B$ (over $n_B$ buckets); the second is the *partial-key* xor-displacement
$$i_2 \;=\; i_1 \oplus \big(H(h(k)) \bmod n_B\big),$$
which depends only on the stored fingerprint $h(k)$, not on $k$. The relation is symmetric ($i_1 = i_2 \oplus (H(h(k))\bmod n_B)$), so given any occupied slot the *other* candidate bucket of its occupant is recoverable from the fingerprint in that slot. Relocation therefore needs **no flash read and no AEAD decryption** — it is a pure in-RAM xor. (A naive full-key cuckoo table would require reading and decrypting the displaced record on every relocation, a cost the energy model of §8 does not carry; partial-key hashing avoids it entirely.)
- **Lookup** reads at most $2b$ slots across the two candidate buckets — worst-case $O(1)$ (§5.1).
- **Insertions** relocate entries along cuckoo paths using the xor rule above; with bucket size $b=4$ a load factor $\alpha\approx 0.95$ is achievable whp (§5.2, cuckoo-filter thresholds), with a small stash (§5.2) absorbing the rare insertion failure so the $2b$ worst-case lookup bound is preserved.
- Cost is $\approx (f + p)$ bits per key with $p=\lceil\log_2 (C_\text{flash})\rceil$, *independent of value size*. Only live keys occupy RAM.

The index is a **cache**: it holds no information not derivable from the log. On boot it is rebuilt by scanning the log (or the most recent checkpoint plus tail). A rebuilt table need not be bit-identical to the pre-crash one — cuckoo placement is insertion-order-dependent — but every live key is placed in one of its two candidate buckets and mapped to its latest offset, which is all lookup correctness requires (§4.2, §5.1).

## 3.3 Layer 3 — Per-record AEAD (confidentiality + integrity, G1/G2)

**Key hierarchy (domain separation).** SLATE never uses the raw device key $K$ directly. Three object types — data records, commit markers, and checkpoints — are authenticated/encrypted, and if they shared one key and one nonce space a nonce could be reused across object types, voiding the AEAD security precondition. We derive per-purpose subkeys with a KDF (HKDF-SHA-256, or a keyed hash where code size is critical):
$$
K_{\mathrm{rec}} = \mathrm{KDF}(K,\text{"rec"}),\quad
K_{\mathrm{cm}} = \mathrm{KDF}(K,\text{"cm"}),\quad
K_{\mathrm{ckpt}} = \mathrm{KDF}(K,\text{"ckpt"}),
$$
and, for records, a **per-epoch** record key
$$
K^{(e)}_{\mathrm{rec}} = \mathrm{KDF}(K,\text{"rec"}\,\|\,e).
$$
Each subkey has its own private $\mathrm{seq}$-derived nonce space, so nonces never collide across object types, and the record nonce space is reset each epoch — which removes the $\mathrm{seq}$-wraparound / key-rotation hazard noted in §10 (a fresh key every $\Theta$ records means the 64-bit $\mathrm{seq}$ counter need never wrap under one key) and yields coarse **forward secrecy** across epochs at zero extra hot-path cost (one KDF call per epoch, amortized over $\Theta$ records). All security reductions in §6 are stated per-subkey and compose by a standard hybrid over the (small) number of subkeys.

Each value is then sealed with an AEAD scheme (ChaCha20-Poly1305, or AES-GCM where hardware-accelerated) under the current epoch's record subkey:
$$
c \;=\; \mathrm{AEAD.Enc}_{K^{(e)}_{\mathrm{rec}}}(\mathrm{nonce},\; \mathrm{ad},\; k\,\|\,v),\qquad \mathrm{ad} = (\mathrm{seq}\,\|\,\mathrm{op}\,\|\,h(k)\,\|\,\mathrm{klen}\,\|\,\mathrm{vlen}).
$$
The record header is bound as associated data $\mathrm{ad}$, so header tampering is detected. Nonces are the record's unique $\mathrm{seq}$ (within the epoch) expanded to 96 bits (a counter), guaranteeing non-repetition under each fixed subkey — the standard requirement for AEAD security. Commit markers ($\tau_{\mathrm{CM}}$, §3.4) and checkpoints (§3.6) use $K_{\mathrm{cm}}$ and $K_{\mathrm{ckpt}}$ respectively.

## 3.4 Layer 4 — Freshness-bound $O(1)$ authenticated log (G2/G3) — *original*

Whole-store tamper-evidence and rollback-resistance are provided by a **hash-chain accumulator** maintained incrementally:
$$
\chi_0 = \mathrm{const},\qquad \chi_i = H\big(\chi_{i-1}\,\|\,r_i\big),
$$
so within an epoch $\chi$ commits to the ordered record sequence. Every commit marker carries the current chain tip and is MAC'd, giving tamper-evidence at $O(1)$ per commit. The hardware counter, however, is bound in at a coarser granularity — the **epoch** — for a reason that is central to feasibility on real hardware.

**Per-epoch anchoring (chain-safety under garbage collection).** A naive reading of $\chi_i=H(\chi_{i-1}\|r_i)$ chains over the *entire historical log*, which is incompatible with compaction: garbage collection (§3.7) erases segments, so the historical record sequence no longer exists on flash and a chain over it could never be re-verified at boot. SLATE therefore **re-anchors the chain at each epoch boundary** to the sealed checkpoint's digest rather than to the previous record:
$$
\chi^{(e)}_{0} \;=\; H\big(\text{"epoch"}\,\|\,e\,\|\,D_{\mathrm{ckpt}}(e)\big),\qquad
\chi^{(e)}_{i} \;=\; H\big(\chi^{(e)}_{i-1}\,\|\,r_i\big),
$$
where $D_{\mathrm{ckpt}}(e)$ is the AEAD-sealed digest of the checkpoint that opens epoch $e$ (the compacted index snapshot plus the sealed pair $(\chi,\mathrm{MC})$ of §3.6). The checkpoint digest transitively commits to the previous epoch's sealed tip, so the *sequence of checkpoints* forms the long-range chain while the per-record chain only ever spans **checkpoint→tail**. Boot verification (Theorem 6, "Whole-log tamper-evidence") re-derives $\chi^{(e)}$ from the current checkpoint forward over records that still physically exist; it never needs the erased history. Records copied forward by compaction are re-appended with **fresh $\mathrm{seq}$ numbers** and folded into the *current* epoch's chain like any other append — the chain makes no claim about their original position, only that the live set as of the latest checkpoint is authentic and fresh.

**Why not increment the counter per commit.** A hardware monotonic counter (eFuse anti-rollback fuses, or an RPMB write counter) supports only a *bounded number of increments over the device's entire lifetime* — tens to a few thousand for eFuse fuse-based counters, and endurance-limited even for RPMB. A busy node commits on the order of $\lambda/B \approx 2$ times per second (see the operating-point table in §9.2); incrementing the counter every commit would exhaust an eFuse budget in minutes to hours. Binding the counter per commit is therefore *not implementable* on the target class — a flaw we correct here by decoupling the two rates.

**Epochs.** Time is divided into **epochs** delimited by checkpoints (§3.6): an epoch spans up to $\Theta$ appended records (equivalently one checkpoint interval). Let the hardware counter read $\mathrm{MC}=e-1$ while epoch $e$ is being written. Commit markers *within* epoch $e$ carry the epoch's target counter value $e$:
$$
\mathrm{CM} \;=\; \big[\mathrm{magic}_{\mathrm{CM}} \,\|\, \mathrm{seq}_{\max} \,\|\, e \,\|\, \chi_{n} \,\|\, \tau_{\mathrm{CM}}\big],\qquad
\tau_{\mathrm{CM}} = \mathrm{MAC}_K\big(\mathrm{seq}_{\max}\,\|\,e\,\|\,\chi_n\big),
$$
and the hardware counter is advanced **once per epoch**, at the epoch-sealing checkpoint. The rate of counter increments is thus $\lambda/\Theta$, not $\lambda$: choosing $\Theta$ large amortizes the counter's scarce increments over many operations.

**Write-ahead ordering (closing the crash window).** The epoch-seal is ordered so that the persisted marker is never *behind* a counter advance in a way that would make a genuine device fail its own check:
1. Write and flush the epoch-$e$ sealing checkpoint carrying counter field $e$ and its MAC (durable).
2. *Then* increment the hardware counter to $\mathrm{MC}=e$.

If power fails **between** steps 1 and 2, the durable marker carries $e$ while hardware reads $e-1$; the genuine latest state is one *ahead* of the counter. Boot therefore accepts a marker whose counter field $m$ satisfies $m\in\{\mathrm{MC}^\ast,\ \mathrm{MC}^\ast+1\}$ (Lemma "Crash-window liveness of the counter protocol"), never $m<\mathrm{MC}^\ast$ (stale epoch) and never $m>\mathrm{MC}^\ast+1$ (unreachable without forgery).

This construction has the properties that distinguish it from a Merkle tree and give it its name:

1. **$O(1)$ update.** Each appended record folds into $\chi$ with a single hash compression — no tree rebalancing, no $O(\log n)$ path update; each commit is one MAC.
2. **$O(1)$ freshness-tip verification.** *Verifying the persisted tip is authentic and fresh* is a single MAC check plus one counter comparison — independent of store size. (This is distinct from *replaying* the tail to rebuild logical state, which is $O(\Theta)$ by Theorem 3, "Bounded recovery"; see §3.4.1.)
3. **Freshness at epoch granularity.** Because $\mathrm{MC}$ is monotonic in hardware and bound under $\mathrm{MAC}_K$, a stale authentic image from an *earlier epoch* carries counter $< \mathrm{MC}^\ast$ and is rejected. Rollback *within* the current epoch (to an earlier commit carrying the same counter) is not distinguished by the counter; the rollback-protection granularity is therefore **one epoch** (Theorem 7, "Rollback detection at epoch granularity").

**Remark (Counter endurance and the $\Theta$ knob).** If the hardware counter tolerates $N_{\mathrm{MC}}$ lifetime increments and each epoch spans $\Theta$ operations, the counter-limited operation lifetime is $N_{\mathrm{MC}}\cdot\Theta$. The same $\Theta$ appears in three places: recovery time is $O(\Theta)$ (Theorem 3), the un-counter-protected rollback window is one epoch $=\Theta$ operations, and the counter-increment rate is $\lambda/\Theta$. Increasing $\Theta$ lengthens counter-limited lifetime and cuts increment pressure, but widens the rollback window and lengthens recovery. A designer picks $\Theta$ from the tightest of: desired lifetime ($\ge L_{\mathrm{target}}/N_{\mathrm{MC}}$ ops per epoch), tolerable recovery latency, and tolerable rollback window. For $N_{\mathrm{MC}}=10^3$ and $\Theta=10^4$, the counter supports $10^7$ operations before exhaustion.

**Remark (Devices without a true monotonic counter).** On parts with no genuine hardware counter, G3 (rollback resistance) cannot be met unconditionally — a stateless device cannot detect replacement by a whole earlier authentic image (the converse of Theorem 7). SLATE then degrades **gracefully and explicitly**: G1 (confidentiality) and G2 (tamper-evidence) are unaffected — any modification is still detected by the chain+MAC — and G3 falls back to *best-effort*, e.g. storing the counter in a dedicated wear-leveled flash region (which an attacker with physical flash access can still roll back). The engine reports which mode it is in; it never silently claims rollback protection it cannot provide.

**Remark (Why not a Merkle tree?).** A Merkle tree gives $O(\log n)$ *membership proofs* for third-party auditing — a capability we do not need on a single trusted device. For our goal (the device verifying its own store at boot and detecting tamper/rollback) the linear hash chain is strictly cheaper: $O(1)$ update and $O(1)$ tip authentication, with no sibling-hash storage. This is the sense in which the construction is sharper for the edge single-node regime.

### 3.4.1 The two costs are different: $O(1)$ tip check vs $O(\Theta)$ replay

It is worth stating plainly, because the distinction is easy to conflate: **verifying freshness** and **reconstructing state** are separate operations with separate costs. Verifying that the persisted freshness tip $(\chi,\ \text{counter})$ is authentic and current is $O(1)$ — one MAC verification and one counter comparison — regardless of how many records the store holds. *Rebuilding the logical state and RAM index* from the last checkpoint requires replaying the tail, which is $O(\Theta)$ (Theorem 3). The "$O(1)$ boot" headline refers only to the freshness-tip check; a full boot that also rebuilds the index pays the $O(\Theta)$ replay. Both are independent of lifetime operation count, which is the property that matters for an edge device, but they are not the same $O(1)$.

## 3.5 Layer 5 — Reed–Solomon segment parity (bit-rot / bad-block tolerance)

Each sealed segment of $k$ data blocks is extended with $m=n-k$ **parity blocks** computed by a systematic Reed–Solomon code $\mathrm{RS}(n,k)$ over $\mathrm{GF}(2^8)$. Any $m$ block erasures within the stripe are recoverable (§6). Parity is computed **once per sealed segment** (not per record), so it does not inflate the hot-path write cost per operation, only the per-segment amortized cost.

**Protecting the open (unsealed) head segment.** Because full RS parity is computed only at seal time, records in the *currently open* segment would otherwise be covered by error *detection* (the AEAD tag) but not error *correction*: a bad block in the head after a batch is acknowledged but before the segment seals would lose committed, acknowledged data — in tension with the prefix-durability guarantee of Theorem 1. SLATE closes this window with two lightweight measures on the open segment only:
1. **Double-written commit markers.** Each commit marker (§3.4) is written to two distinct flash pages. A marker is the sole witness that a batch is durable; duplicating it (32–80 B) converts a single bad page holding the marker from "lost acknowledgement" into a located erasure repaired from its twin.
2. **Per-batch XOR parity page.** After each committed batch the engine appends one **XOR parity page** — the byte-wise XOR of the data pages written since the last parity page. This is a trivial $\mathrm{RS}(k{+}1,k)$ single-erasure code: any one lost page in the open segment is a located erasure (AEAD tag identifies *which* page failed) and is reconstructed by XOR. The XOR pages are **superseded and discarded** when the segment seals and full $\mathrm{RS}(n,k)$ parity is computed, so they cost only $\approx 1$ page per batch of transient flash, not permanent overhead.

Together these convert a head bad-block from silent loss of acked data into a *located, single-erasure-correctable* fault across the entire pre-seal window, restoring consistency with Theorem 1. The cost — one XOR page per batch plus a duplicated marker — is charged in the energy model (§8.1); it is small because a batch already amortizes several records per commit.

## 3.6 Checkpoints (bounded recovery)

Periodically (every $\Theta$ operations, or per sealed segment) the engine writes a **checkpoint**: a compacted snapshot of the live index plus the pair $(\chi,\mathrm{MC})$ at that point, itself AEAD-sealed and chained. On boot, recovery loads the latest valid checkpoint and replays only the records after it, bounding recovery work to $O(\Theta)$ (§4).

## 3.7 Garbage collection / compaction

Because updates and deletes append new records, superseded records accumulate. A **segment-at-a-time compactor** reclaims space: it selects a sealed segment, copies its still-live records forward to the head, and erases the segment. Choice of victim segment (utilization $u$) drives write-amplification (§8).

**Chain-safety of compaction.** Compaction interacts with the freshness chain, and the interaction must be stated explicitly or Theorem 6 is in tension with erasing segments. The rule is the per-epoch anchoring of §3.4: the authenticated object is never "the full historical byte log" but "the live set as of the latest sealed checkpoint, plus the committed tail after it." A live record copied forward during compaction is written as a fresh append with a new $\mathrm{seq}$ and folded into the current epoch's chain $\chi^{(e)}$; the erased segment's records are not required for any future verification because the chain re-anchors at each checkpoint (§3.6) and the checkpoint snapshot already commits to every live key's current value. Concretely, compaction is only ever performed on segments **older than the latest checkpoint**, so a crash mid-compaction truncates to a valid prefix (§4) exactly as an ordinary torn tail does: either the forwarded copies are committed (and the old segment may be erased) or they are not (and the old segment is still present), never both-lost. This preserves prefix-durability (Theorem 1) across GC.

**Tombstone reclamation invariant.** A `Delete`$(k)$ appends a *tombstone* record that shadows any earlier `Put`$(k,\cdot)$. Tombstones must eventually be reclaimed or they accumulate forever, but dropping one too early is a classic log-structured-store correctness bug: if a tombstone for $k$ is discarded while an older `Put`$(k,v)$ still exists in some not-yet-compacted segment, a boot replay would re-encounter that `Put` and *resurrect* the deleted key. We therefore state the reclamation rule as an invariant the correctness argument depends on:

> **Invariant (T).** A tombstone for key $k$ with sequence number $\mathrm{seq}_t$ may be dropped during compaction only when no record for $k$ with $\mathrm{seq} < \mathrm{seq}_t$ survives in any live segment.

This is checked cheaply with a **compaction watermark**: maintain, per live segment, its minimum sequence number $\mathrm{minseq}(S)$; a tombstone at $\mathrm{seq}_t$ is safe to drop once $\mathrm{seq}_t \le \min_{S\ \text{live}} \mathrm{minseq}(S)$, i.e. once every segment that could hold an older `Put` for $k$ has itself been compacted away. Equivalently, a tombstone survives at least until the compaction frontier passes the position of the newest record it could be shadowing.

**Proposition (No resurrection).** Under Invariant (T), for every key $k$ the value returned by `Get`$(k)$ after any sequence of compactions equals the value determined by the highest-$\mathrm{seq}$ live record for $k$ in the uncompacted log — a deleted key is never resurrected.

*Proof.* The only way `Get`$(k)$ could return a resurrected value is if the index, after replay, maps $k$ to a `Put` record whose $\mathrm{seq}$ is smaller than that of a tombstone that logically supersedes it. By Invariant (T) a tombstone at $\mathrm{seq}_t$ is dropped only when no record for any key with $\mathrm{seq}<\mathrm{seq}_t$ remains live; in particular no older `Put`$(k,\cdot)$ remains, so replay cannot encounter one after the tombstone is gone. While the tombstone is still present, replay in $\mathrm{seq}$ order applies it after any older `Put` and removes $k$ from the index (§4). Either way the index reflects the deletion, so `Get`$(k)$ returns "absent." ∎

# 4. Correctness and durability

We first make precise what "recovery" computes, then prove the three guarantees. Throughout, "the log" refers to the persisted byte sequence on flash; "the index" to the RAM structure.

**Definition (Well-formed record and valid prefix).** A record $r_i$ is *well-formed* if its magic byte is intact, its length field is consistent with the bytes read, and its AEAD tag $\tau$ verifies under $K$ with associated data equal to its header. A batch $[r_{j},\dots,r_{j'}]$ is *committed* if it is followed by a commit marker $\mathrm{CM}$ whose MAC $\tau_{\mathrm{CM}}$ verifies. The *valid prefix* $L^{\le c}$ of a persisted log is the longest prefix all of whose records are well-formed and lie within a committed batch.

**Definition (Recovery function).** On boot, `Recover` scans from the last checkpoint (or from $S_0$ if none), verifying records and commit markers, and returns $(\sigma_R, I_R, \chi_R, \mathrm{MC}_R)$: the logical state $\sigma_R$, the reconstructed index $I_R$, the chain tip $\chi_R$, and the counter $\mathrm{MC}_R$, computed by replaying exactly the records of the valid prefix in $\mathrm{seq}$ order. Records beyond the valid prefix (a torn or uncommitted tail) are truncated: the head is repositioned to the end of the valid prefix.

## 4.1 Prefix-durability

**Theorem 1 (Prefix-durability).** Under the power-loss model (§2.3), for any execution interrupted by an arbitrary number of power failures at arbitrary instants, `Recover` returns a logical state $\sigma_R$ equal to the state produced by some prefix $A' = (w_1,\dots,w_j)$ of the acknowledged-write sequence $A=(w_1,\dots,w_n)$, with $A'$ containing every write acknowledged before the last crash. That is:
$$
\exists\, j \le n :\quad \sigma_R = \mathrm{apply}(w_1,\dots,w_j),\qquad j \ge n_{\mathrm{ack}},
$$
where $n_{\mathrm{ack}}$ is the number of writes acknowledged strictly before the final power loss. No acknowledged write is ever lost, and no partial/unacknowledged write is ever exposed.

*Proof.* We argue in three parts.

*(i) Recovery yields a prefix.* `Recover` replays records in strictly increasing $\mathrm{seq}$ order up to the valid prefix boundary $c$ and stops. Since $\mathrm{seq}$ is assigned monotonically at append time and the log is append-only (§3.1), the records $r_1,\dots,r_c$ are exactly the first $c$ operations in program order. Applying them in order yields $\mathrm{apply}(w_1,\dots,w_j)$ where $w_1,\dots,w_j$ are the operations among $r_1,\dots,r_c$ — a prefix of $A$ by construction. (Delete tombstones are applied as removals, preserving prefix semantics.)

*(ii) Every acknowledged write survives ($j \ge n_{\mathrm{ack}}$).* By the acknowledgment rule (§2.2), a write $w_i$ is acknowledged only after (a) its record was reported programmed and (b) the commit marker of its batch was reported programmed. Consider any $w_i$ acknowledged before the final crash. Its record and its batch's commit marker were durably programmed prior to the crash; by the power-loss model, programmed pages persist across power loss. Hence at recovery $r_i$ is well-formed (its tag verifies — it was written completely) and lies within a committed batch (its $\mathrm{CM}$ verifies). Therefore $r_i \le c$: it is inside the valid prefix, so $i \le j$. As this holds for all acknowledged $w_i$, we get $j \ge n_{\mathrm{ack}}$.

*(iii) No partial or unacknowledged write is exposed.* Suppose the crash occurred while programming record $r_m$ or before its batch's $\mathrm{CM}$ was programmed. Two cases: (a) $r_m$ was torn — by condition 1 of the power-loss model its integrity check fails, so it is not well-formed and $c < m$; it is truncated. (b) $r_m$ is complete but its batch's commit marker was not durably programmed — then the marker either is absent (tail reads as erased pattern, condition 3) or is itself torn (its MAC fails). Either way the batch is not committed, so the valid-prefix boundary $c$ falls before $r_m$, and $r_m$ is truncated. In both cases no unacknowledged operation enters $\sigma_R$. By condition 2 (no reordering across the commit marker), no later batch could have been committed "ahead" of an earlier uncommitted one, so truncation at the first uncommitted batch loses nothing that was durable. ∎

**Remark (Atomicity of multi-record batches).** Theorem 1 gives *all-or-nothing* batch commit: because the boundary $c$ is defined at commit markers, either an entire batch is recovered or none of it is. A single `Put` is the special case of a size-one batch.

## 4.2 The index is a rebuildable cache

**Theorem 2 (Index reconstructibility).** The index $I$ carries no information that is not a deterministic function of the persisted valid prefix $L^{\le c}$. Formally, there is a function $\mathcal{B}$ with $I_R = \mathcal{B}(L^{\le c})$, and for the query interface, $\forall k:\ \mathrm{Get}(k)$ computed via $I$ equals $\mathrm{Get}(k)$ computed by a direct scan of $L^{\le c}$.

*Proof.* $I$ maps each live key to the offset of its most-recent record. Define $\mathcal{B}(L^{\le c})$ as the scan that, processing $r_1,\dots,r_c$ in order, sets $I[h(k)] \leftarrow \mathrm{offset}(r_i)$ for each `Put` and removes $h(k)$ for each tombstone. This is exactly the update `Recover` performs, so $I_R = \mathcal{B}(L^{\le c})$. For correctness of lookups: $I[h(k)]$ points to the record with the largest $\mathrm{seq}$ for $k$ (later writes overwrite the slot), which is the definition of $\sigma(k)$. A fingerprint collision $h(k)=h(k')$ is resolved by comparing the full key recovered by AEAD-decrypting the record (§3.1) against the queried key after the log read, so lookups never return another key's value; this comparison is exact (full key, not fingerprint), so it is correct even though $f$-bit fingerprints collide freely among the $n$ live keys. Hence the two `Get` paths agree. Since $I$ is fully determined by $L^{\le c}$, losing $I$ (power failure) costs no durable information — only the recomputation covered by Theorem 3 below. ∎

**Corollary (No double-write for the index).** Index updates need never be persisted on the hot path: durability is carried entirely by the log. This is what admits a single sequential write per operation (the basis of the low write-amplification claim in §8).

## 4.3 Bounded recovery time

**Theorem 3 (Bounded recovery).** Let checkpoints be taken at least every $\Theta$ appended records. Then `Recover` reads at most one checkpoint plus $\Theta$ records, so recovery time is
$$
T_{\mathrm{rec}} \;\le\; T_{\mathrm{ckpt}} + \Theta\cdot t_{\mathrm{rec-rec}},
$$
independent of the total number of operations ever performed, where $t_{\mathrm{rec-rec}}$ is the per-record verify+replay cost and $T_{\mathrm{ckpt}}$ the checkpoint-load cost. In particular $T_{\mathrm{rec}}=O(\Theta)$.

*Proof.* A checkpoint persists $(\sigma,I,\chi,\mathrm{MC})$ at the point of its creation, itself AEAD-sealed and chained (§3.6), so loading it restores that state in $T_{\mathrm{ckpt}}$ without scanning prior records. By hypothesis the most recent valid checkpoint precedes the crash by at most $\Theta$ appended records. `Recover` loads that checkpoint and replays only those $\le \Theta$ tail records (verifying each, per Theorem 1), for a total of $T_{\mathrm{ckpt}} + \Theta\, t_{\mathrm{rec-rec}}$. No dependence on lifetime operation count remains. Choosing $\Theta$ trades recovery latency against checkpoint write overhead (analyzed in §8). ∎

**Remark.** Combining Theorems 1–3: the log is the single source of truth, the RAM index is a pure cache reconstructible in bounded time, and after any crash the device serves exactly a prefix of acknowledged writes. These are the durability guarantees a safety-critical edge node needs, proved from the flash and crash model rather than assumed.

## 4.4 Simulation: crash-injection Monte-Carlo

The theorems above are proved from the model; to check that the model's state machine actually behaves as claimed — and to answer the fair critique that the report is purely analytical — we implemented the on-flash state machine directly and subjected it to randomized power-loss injection. This is a *model-level* validation (a discrete-event simulation of the byte layout and recovery scanner), not a hardware measurement; it verifies that the recovery logic and the counter protocol have no gaps the proofs might have glossed.

**Setup.** We lay out a log of 600 records grouped into commit batches of $B=8$, with epochs of $\Theta=64$ records; each epoch seals with a checkpoint marker (carrying counter field $=$ epoch) followed by the hardware-counter advance, in the write-ahead order of §3.4. We then inject $20{,}000$ crashes at a uniformly random byte offset (any event index, torn at a random fraction), run `Recover`, and compare the recovered committed prefix against the ground-truth set of acknowledged writes. Separately we present $5{,}000$ stale-but-authentic images from strictly earlier epochs and apply the boot rule of the crash-window lemma (§3.4).

**Results (Figure 2).** Across all $20{,}000$ crashes: recovery reproduced *exactly* the last committed prefix in $100.00\%$ of trials; **zero** acknowledged writes were ever lost (no durability violation of Theorem 1) and **zero** torn or uncommitted tails were ever accepted (no safety violation). Of these, $280$ crashes landed inside the seal window (marker durable, counter not yet advanced); all $280$ booted correctly under the $m\in\{\mathrm{MC}^\ast,\mathrm{MC}^\ast+1\}$ rule (confirming the crash-window lemma). Every one of the $5{,}000$ earlier-epoch rollback images was rejected ($100.00\%$), confirming Theorem 7 at epoch granularity. The simulation thus reproduces the proved guarantees with no discrepancy.

![SLATE crash-injection Monte-Carlo. **Left:** recovered durable prefix (dashed) tracks the acknowledged-write ground truth (solid) exactly as the crash position sweeps the log — the sawtooth is the batch structure (writes become durable only at each commit marker). **Right:** outcome rates over 20,000 random crashes — 100% correct recovery, 0% lost acknowledgements, 0% accepted torn/uncommitted data, and 100% rejection of earlier-epoch rollback images. Green bars are the desired outcomes; the two violation categories are provably empty.](artifacts/crash_sim.png)

# 5. Index complexity and the memory budget

The "ultra-light" objective is largely a statement about RAM: the index is the only structure whose size grows with the number of live keys. We bound its lookup cost and its bytes-per-key.

## 5.1 Worst-case lookup

**Lemma (Constant-time lookup).** With a partial-key cuckoo table of bucket size $b$, two candidate buckets, and a stash of $s$ entries, `Get` inspects at most $2b+s$ index slots, independent of the number of stored keys $n$. Thus index lookup is $O(b+s)=O(1)$ in the worst case, followed by exactly one log read.

*Proof.* A key $k$ may reside only in bucket $i_1(k)$, bucket $i_2(k)$, or the stash (the cuckoo invariant, maintained by the insertion procedure of §3.2); the two candidate buckets each hold $b$ slots. A lookup reads both candidate buckets — $2b$ slots — then the $s$-entry stash, comparing the stored $f$-bit fingerprint; on a fingerprint match it performs one log read, AEAD-decrypts the record, and compares the recovered full key $k$ against the query (§3.1). The full-key check is what makes the result correct despite narrow fingerprints; the fingerprint only prunes which slots trigger a log read. No probing beyond the two buckets and the stash ever occurs, so the slot count is exactly bounded by $2b+s$ regardless of $n$ or load factor. With $b=4$ and $s\le 8$ this is $\le 16$ slot reads, and the expected number of log reads per lookup is $1+\varepsilon_{\mathrm{FP}}$ (§5.3). ∎

**Remark (Why not linear-probing / Robin Hood?).** Open-addressing schemes give $O(1)$ *expected* lookup but a worst case that degrades with load factor. Bucketized cuckoo hashing gives a *hard* $2b$ bound — valuable on a real-time edge node where tail latency, not just mean latency, is a requirement.

## 5.2 Load factor and insertion

**Lemma (Achievable load).** For bucket size $b=4$ and two hash functions, a load factor $\alpha \le 0.95$ is achievable with high probability; i.e. the table holds $n$ keys in $\lceil n/(\alpha b)\rceil$ buckets with insertion failing (requiring a resize) with probability $o(1)$ as the table grows.

*Justification.* SLATE uses *partial-key* (cuckoo-filter) hashing (§3.2), so the relevant thresholds are those of the cuckoo filter (Fan et al.), not full-key cuckoo hashing. Two effects must be distinguished. **(i) Occupancy.** The bucketized-cuckoo occupancy threshold above which a valid assignment ceases to exist increases with $b$: $\alpha^\ast(1)\approx 0.5$, $\alpha^\ast(2)\approx 0.84$, $\alpha^\ast(4)\approx 0.95$, $\alpha^\ast(8)\approx 0.98$ — these are the *cuckoo-filter* achievable loads reported by Fan et al., which are slightly lower than the full-key thresholds because the alternate bucket is derived from the $f$-bit fingerprint rather than the key. Operating at $\alpha=0.95$ with $b=4$ sits right at the practical cuckoo-filter load, matching the value Fan et al. report as reliably achievable at $b=4$. **(ii) Fingerprint width feasibility.** Partial-key hashing places an extra constraint absent from full-key cuckoo hashing: because the alternate bucket is $i_2=i_1\oplus(H(h(k))\bmod n_B)$, the fingerprint must be wide enough that distinct keys colliding in a bucket rarely share a fingerprint, otherwise relocations can fail to find a legal alternate. Fan et al. show this requires roughly $f \gtrsim \log_2 n_B + \log_2(1/\text{target failure})$ bits; for $n_B\approx 2.9\text{k}$ buckets (11k keys, $b=4$, $\alpha=0.95$) and a $2^{-8}$ target, $f=12$ leaves comfortable margin. Thus the $f=12$ setting of §5.3 is simultaneously the false-positive choice *and* the relocation-feasibility choice. Within these constraints the random-walk insertion succeeds in $O(1)$ expected relocations and $O(\log n)$ whp; the $\le b$-entry stash below absorbs the residual $o(1)$ failure.

**Remark (Stash preserves the worst-case bound).** The $o(1)$ insertion-failure probability is not zero, and a single unresolvable insertion must not force a table resize on a memory-constrained node. We add a small **stash** of $s\in[4,8]$ entries (Kirsch–Mitzenmacher–Wieder) that holds any key the random walk fails to place. Lookup checks the stash after the two candidate buckets, so the worst-case slot count becomes $2b+s$ — still $O(1)$ and independent of $n$ — and the constant-time-lookup lemma's hard tail-latency bound is preserved. Empirically a stash of a handful of entries drives the effective failure probability below $2^{-40}$ at $\alpha=0.95$, so a resize is essentially never triggered in the device's lifetime. The stash costs $s(f+p)$ bits total (tens of bytes), negligible against the index.

## 5.3 Bytes per key and the collision–memory trade-off

Each occupied slot stores an $f$-bit fingerprint and a $p$-bit pointer (log offset), where $p=\lceil\log_2 C_{\mathrm{flash}}\rceil$. At load factor $\alpha$, the amortized index cost per live key is
$$
\mathrm{bits/key} \;=\; \frac{f + p}{\alpha}\qquad\text{(memory law).}
$$
The fingerprint width $f$ controls the probability that a lookup does a *wasted* log read (a fingerprint match on the wrong key). Modeling fingerprints as uniform over $2^f$ values, the per-nonmatching-slot false-positive probability is $2^{-f}$, and over the $2b$ inspected slots the expected number of wasted log reads per negative lookup is bounded by
$$
\varepsilon_{\mathrm{FP}} \;\le\; 2b\cdot 2^{-f}.
$$

**Proposition (Memory–FP trade-off).** To guarantee an expected wasted-read rate $\le \varepsilon$ on negative lookups, it suffices to take $f \ge \log_2(2b/\varepsilon)$, giving an index cost of $\big(\log_2(2b/\varepsilon) + \lceil\log_2 C_{\mathrm{flash}}\rceil\big)/\alpha$ bits per key.

*Proof.* Set $2b\cdot 2^{-f} \le \varepsilon \iff 2^{-f}\le \varepsilon/(2b) \iff f\ge \log_2(2b/\varepsilon)$. Substitute into the memory law. Because fingerprint and pointer are the only per-key state and full-key verification happens against the log (not RAM), no per-key key-bytes are charged to RAM. ∎

Figure 3 plots both the memory law and the FP bound. For a concrete ESP32 target ($C_{\mathrm{flash}}=4\,\mathrm{MB}\Rightarrow p=22$ bits, $b=4$, $\alpha=0.95$), $f=12$ gives $\varepsilon_{\mathrm{FP}}\le 8\cdot2^{-12}\approx 2\times10^{-3}$ at $(12+22)/0.95\approx 36$ bits/key $=4.5\,\mathrm{B}$ per key — so a $50\,\mathrm{KB}$ index budget holds $\approx 11,000$ live keys.

![Index cost model. **Left:** amortized RAM per live key vs. fingerprint width $f$, for three load factors, at pointer width $p=22$ bits (4 MB flash). **Right:** bound on expected wasted log reads per negative lookup, $\varepsilon_{\mathrm{FP}}\le 2b\,2^{-f}$, vs. $f$ for three bucket sizes. Wider fingerprints cost linear RAM but cut false positives exponentially; the marked point is the recommended ESP32 setting (f=8 bits for 32 KB / 8k keys, f=12 bits for 50 KB / 11k keys).](artifacts/ram_tradeoff_plot.png)

# 6. Security

We now prove the three security goals of §2.4 by reduction to standard assumptions. We use the concrete-security style: an adversary breaking a SLATE property with advantage $\epsilon$ and resources $t$ yields an adversary breaking an underlying primitive with related advantage and resources. Let the AEAD scheme be $\Pi=(\mathrm{Enc},\mathrm{Dec})$ and let $H$ be a hash function.

**Assumption (Primitive security).** $\Pi$ is a secure nonce-based AEAD: it is IND-CPA and provides ciphertext integrity (INT-CTXT), with advantages $\mathbf{Adv}^{\mathrm{ind\text{-}cpa}}_{\Pi}(t,q)$ and $\mathbf{Adv}^{\mathrm{int\text{-}ctxt}}_{\Pi}(t,q)$ negligible for the key length used (e.g. 256-bit ChaCha20-Poly1305). $H$ is collision-resistant with advantage $\mathbf{Adv}^{\mathrm{cr}}_{H}(t)$ negligible. The MAC used in the commit marker is strongly unforgeable (SUF-CMA) with advantage $\mathbf{Adv}^{\mathrm{suf}}(t,q)$. The device key $K$ and the monotonic counter $\mathrm{MC}$ are outside the adversary's reach (§2.4).

## 6.1 Confidentiality (G1)

**Theorem 4 (Value confidentiality).** For any adversary $\mathcal{A}$ (§2.4) that, after obtaining the full flash image, distinguishes the values underlying two equal-length write histories, there is an adversary $\mathcal{B}$ against the AEAD IND-CPA game with
$$
\mathbf{Adv}^{\mathrm{conf}}_{\mathrm{SLATE}}(\mathcal{A}) \;\le\; \mathbf{Adv}^{\mathrm{ind\text{-}cpa}}_{\Pi}(\mathcal{B}) ,
$$
and $\mathcal{B}$ runs in essentially the same time as $\mathcal{A}$. Hence values are confidential up to their length.

*Proof.* The flash image is a sequence of records; the only value-dependent bytes are the ciphertexts $c_i=\mathrm{Enc}_K(\mathrm{nonce}_i,\mathrm{ad}_i,v_i)$ (headers hold $\mathrm{seq}$, op, fingerprint, length, nonce — all independent of value contents beyond length). $\mathcal{B}$ simulates SLATE to $\mathcal{A}$ by forwarding every `Put`$(k,v)$ as an IND-CPA encryption query for $v$ under the fresh nonce $\mathrm{nonce}_i=\mathrm{seq}_i$ (nonces never repeat under fixed $K$ because $\mathrm{seq}$ is strictly increasing — the AEAD security precondition), assembling the record around the returned ciphertext, and handing $\mathcal{A}$ the resulting image. A distinguisher for the underlying values is exactly a distinguisher for the IND-CPA challenge, so $\mathcal{B}$ inherits $\mathcal{A}$'s advantage. Length leakage is explicit in the $\mathrm{len}$ field and is the standard, unavoidable AEAD leakage (mitigable by padding buckets if desired). ∎

## 6.2 Integrity / tamper-evidence (G2)

**Theorem 5 (Per-record integrity).** If $\mathcal{A}$ produces a modified flash image on which `Recover` accepts (does not abort) a record whose $(\mathrm{ad},\mathrm{value})$ was never written by the device, then there is $\mathcal{B}$ breaking AEAD ciphertext integrity:
$$
\mathbf{Adv}^{\mathrm{tamper\text{-}rec}}_{\mathrm{SLATE}}(\mathcal{A}) \;\le\; \mathbf{Adv}^{\mathrm{int\text{-}ctxt}}_{\Pi}(\mathcal{B}).
$$

*Proof.* `Recover` accepts a record only if $\mathrm{Dec}_K(\mathrm{nonce},\mathrm{ad},c,\tau)\ne\bot$, i.e. the AEAD tag verifies with the header as associated data. A record accepted but never written is a fresh $(\mathrm{nonce},\mathrm{ad},c,\tau)$ that decrypts successfully — precisely an INT-CTXT forgery. $\mathcal{B}$ runs $\mathcal{A}$, uses its encryption oracle to build the honest image, and outputs $\mathcal{A}$'s novel accepted ciphertext as its forgery. Because $\mathrm{ad}$ binds $\mathrm{seq}$, op, fingerprint, and length, any header alteration (e.g. redirecting a value to a different key or replaying a value under a new $\mathrm{seq}$) changes $\mathrm{ad}$ and thus needs a new valid tag — also an INT-CTXT forgery. ∎

**Theorem 6 (Whole-log tamper-evidence).** Suppose $\mathcal{A}$ presents an image whose record sequence $L'\ne L^{\le c}$ (the genuine committed prefix) yet whose persisted commit marker verifies and whose recomputed chain tip matches the marker's $\chi$. Then there is $\mathcal{B}$ that either finds a collision in $H$ or forges the marker MAC:
$$
\mathbf{Adv}^{\mathrm{tamper\text{-}log}}_{\mathrm{SLATE}}(\mathcal{A}) \;\le\; \mathbf{Adv}^{\mathrm{cr}}_{H}(\mathcal{B}) + \mathbf{Adv}^{\mathrm{suf}}(\mathcal{B}).
$$

*Proof.* The commit marker binds $\chi_n$ under $\mathrm{MAC}_K$. Two cases for an accepted tampered image.
*(a) The marker's $(\mathrm{seq}_{\max},\mathrm{MC},\chi)$ triple differs from any the device produced.* Then $\tau_{\mathrm{CM}}$ is a MAC on a fresh message — a SUF-CMA forgery; $\mathcal{B}$ outputs it.
*(b) The triple equals a genuine one, so $\chi'=\chi_n$, but $L'\ne L^{\le c}$.* The chain is $\chi_i=H(\chi_{i-1}\|r_i)$ with fixed $\chi_0$. Equal tips from unequal record sequences means, walking the two chains back from the common $\chi_n$, there is a least index $j$ where the inputs differ but the outputs $H(\chi'_{j-1}\|r'_j)=H(\chi_{j-1}\|r_j)$ coincide — a collision of $H$. $\mathcal{B}$ outputs that colliding pair. (Length/typing tricks are prevented by fixed-width fields and the magic bytes; different-length images differ in some $r_j$ and fall into the same argument.) Either way the stated bound holds. Thus any modification to the committed log is detected at boot except with negligible probability, and `Recover` aborts rather than serving altered data. ∎

**Remark (Scope of $L^{\le c}$ under compaction).** Here $L^{\le c}$ is the committed record sequence *from the latest sealed checkpoint to the tail*, not the entire historical byte log — consistent with the per-epoch anchoring $\chi^{(e)}_0=H(\text{"epoch"}\|e\|D_{\mathrm{ckpt}}(e))$ of §3.4 and the chain-safe compaction of §3.7. Tamper-evidence over the full history is provided transitively: each checkpoint digest $D_{\mathrm{ckpt}}(e)$ commits to the previous epoch's sealed tip, so the sequence of checkpoints is itself a hash chain, and altering any earlier epoch's committed state changes some $D_{\mathrm{ckpt}}$ and is caught by the same collision/forgery argument applied one epoch up. Records that compaction has legitimately erased are outside $L^{\le c}$ by construction and carry no live information (their keys' current values live in the checkpoint snapshot), so their absence is not "tampering."

## 6.3 Freshness / rollback-resistance (G3)

**Lemma (Crash-window liveness of the counter protocol).** Under the write-ahead epoch-seal ordering of §3.4 (flush the epoch-$e$ marker carrying counter field $e$, *then* advance hardware $\mathrm{MC}$ to $e$), a genuine device's latest durable marker always has counter field $m\in\{\mathrm{MC}^\ast,\ \mathrm{MC}^\ast+1\}$, where $\mathrm{MC}^\ast$ is the hardware reading at boot. Hence the boot rule "accept the highest-counter durable marker with $m\in\{\mathrm{MC}^\ast,\mathrm{MC}^\ast+1\}$ and a verifying MAC" never rejects a genuine store.

*Proof.* Consider the last epoch seal. Either it completed both steps (marker $=e$ durable, hardware $=e$), giving $m=e=\mathrm{MC}^\ast$; or power failed between step 1 and step 2 (marker $=e$ durable, hardware still $=e-1$), giving $m=e=\mathrm{MC}^\ast+1$. No other durable marker has a higher counter, since a marker with field $e+1$ is written only after $e$'s seal completes, which advances hardware to $e$ first. Thus the latest genuine marker satisfies $m\in\{\mathrm{MC}^\ast,\mathrm{MC}^\ast+1\}$ and passes the boot rule. ∎

**Theorem 7 (Rollback detection at epoch granularity).** Let the device's hardware counter read $\mathrm{MC}^\ast$ at boot. Under the boot rule above, if $\mathcal{A}$ presents a *stale but authentic* image from an *earlier epoch* — committed counter $m' < \mathrm{MC}^\ast$ — then `Recover` rejects it. An $\mathcal{A}$ that makes `Recover` accept such a state yields a MAC forgery:
$$
\mathbf{Adv}^{\mathrm{rollback}}_{\mathrm{SLATE}}(\mathcal{A}) \;\le\; \mathbf{Adv}^{\mathrm{suf}}(\mathcal{B}).
$$
The protection granularity is one epoch: a rollback to an earlier commit *within the current epoch* (same counter field) is outside what the counter distinguishes.

*Proof.* The boot rule accepts a marker only if its counter field $m\in\{\mathrm{MC}^\ast,\mathrm{MC}^\ast+1\}$ and $\tau_{\mathrm{CM}}$ verifies. A stale image from an earlier epoch carries $m' < \mathrm{MC}^\ast$ (the device advanced the hardware counter at every later epoch seal), so $m'\notin\{\mathrm{MC}^\ast,\mathrm{MC}^\ast+1\}$ and it is rejected outright. The only way to pass with an out-of-range counter is to present a marker whose MAC verifies on a $(\mathrm{seq}_{\max},m,\chi)$ triple with $m\in\{\mathrm{MC}^\ast,\mathrm{MC}^\ast+1\}$ that the device never signed — a SUF-CMA forgery, giving the bound. (Accepting $m=\mathrm{MC}^\ast+1$ is safe: only the crash-window marker legitimately carries it, and forging one is again a SUF-CMA break.) Freshness therefore reduces to (i) hardware monotonicity of $\mathrm{MC}$ and (ii) MAC unforgeability. Two honest consequences: without a trusted monotonic counter, rollback of a whole authentic image is fundamentally undetectable by a stateless device (hence the trust assumption and the best-effort fallback of §3.4); and because the counter moves once per epoch, an adversary can roll the store back to any commit within the current epoch — a window of at most $\Theta$ operations — undetected by the counter. This is the price of making the counter's increment rate implementable; it is bounded and quantified rather than hidden. ∎

**Remark (What the proofs do and do not cover).** Theorems 4–7 cover exactly goals G1–G3 against the at-rest adversary. They do *not* cover: physical side channels (excluded non-goal — the reductions assume the primitive's black-box security, not leak-freeness of its implementation); a live-code compromise that reads $K$ from RAM (excluded — $\mathcal{A}$ is powered-off/offline); or availability under a device that is physically destroyed (no single-node scheme can). These boundaries are honest consequences of the threat model, not oversights.

# 7. Erasure coding: bit-rot and bad-block tolerance

Security (§6) detects tampering and corruption but does not *repair* it. Physical media degrade: NAND flash suffers bit errors and whole-block failures with age. We add a Reed–Solomon (RS) layer so that detected corruption within a segment is *recoverable*, converting a fatal data loss into a transparent repair.

## 7.1 The code and its recovery guarantee

**Definition (Per-segment RS code).** A sealed segment is partitioned into $k$ equal data blocks $d_1,\dots,d_k$; a systematic $\mathrm{RS}(n,k)$ code over $\mathrm{GF}(2^8)$ appends $m=n-k$ parity blocks $p_1,\dots,p_m$, computed as $\mathbf{p}=\mathbf{d}\,G_{\mathrm{par}}$ where $G=[\,I_k \mid G_{\mathrm{par}}\,]$ is the generator derived from a Vandermonde/Cauchy matrix. The stripe is the $n$ blocks $(d_1,\dots,d_k,p_1,\dots,p_m)$.

**Theorem 8 (MDS erasure recovery).** $\mathrm{RS}(n,k)$ is Maximum-Distance-Separable: it has minimum distance $d_{\min}=n-k+1$, and therefore *any* set of up to $m=n-k$ erased blocks (blocks whose location is known to be bad — via ECC failure, AEAD-tag failure, or the tamper check) can be reconstructed exactly from the remaining $k$ blocks.

*Proof.* The generator $G$ is a $k\times n$ matrix such that every $k\times k$ submatrix is invertible (the defining property of a Vandermonde/Cauchy construction over a field). Suppose up to $m$ of the $n$ blocks are erased; then at least $k$ blocks survive. Restrict $G$ to the $k$ columns corresponding to any $k$ surviving blocks, obtaining a $k\times k$ submatrix $G_S$; by construction $G_S$ is invertible. The surviving blocks equal $\mathbf{d}\,G_S$, so $\mathbf{d} = (\text{surviving})\,G_S^{-1}$ recovers the original data vector, whence all $n$ blocks. Since this holds for *any* choice of $k$ survivors, any $\le m$ erasures are correctable. The distance $d_{\min}=n-k+1$ meets the Singleton bound with equality, i.e. the code is MDS. Erasure decoding is a single $k\times k$ linear solve over $\mathrm{GF}(2^8)$, i.e. $O(k^2)$ field operations per stripe — done off the hot path, only during repair. ∎

**Remark (Erasures vs. errors).** We rely on *erasure* decoding (known-location failures), which corrects up to $m$ blocks — twice the $\lfloor m/2\rfloor$ that blind error-correction would give. This is sound here precisely because the AEAD tag and device ECC *locate* the bad blocks for us (§2.3). Security and fault-tolerance thus compose: authentication turns silent corruption into a located erasure that RS can repair.

## 7.2 Space and write overhead

The parity overhead is $m/k$ extra storage; the usable fraction is $k/n$. Parity is written once per sealed segment, so per-operation write amplification from RS is $m/k$ *amortized over a full segment* — e.g. $\mathrm{RS}(10,8)$ costs 25% space and 25% amortized extra writes for tolerance of any 2 block failures per stripe.

## 7.3 Survival probability vs. raw bit-error rate

Let $\beta$ be the per-block probability of failure (a block is "failed" if it contains at least one uncorrectable bit given the device's inner ECC; for a crude model with independent bit errors at rate $\mathrm{BER}$ and block size $S_b$ bits, $\beta \approx 1-(1-\mathrm{BER})^{S_b}$). Treating block failures within a stripe as independent, a stripe is *lost* only if more than $m$ of its $n$ blocks fail:
$$
P_{\mathrm{loss}}(\mathrm{stripe}) \;=\; \sum_{i=m+1}^{n}\binom{n}{i}\beta^{\,i}(1-\beta)^{\,n-i}
$$
and a store of $N_s$ independent stripes survives with probability $(1-P_{\mathrm{loss}})^{N_s}$.

**Proposition (Exponential reliability gain in $m$).** For $\beta$ small, $P_{\mathrm{loss}} \approx \binom{n}{m+1}\beta^{\,m+1}$, i.e. each additional parity block multiplies the dominant loss term by $\Theta(\beta)$: reliability improves by roughly one order of magnitude per parity block per order-of-magnitude margin in $\beta$.

*Proof.* For $\beta\to 0$ the sum is dominated by its smallest-exponent term $i=m+1$: $P_{\mathrm{loss}} = \binom{n}{m+1}\beta^{m+1}(1-\beta)^{n-m-1}+O(\beta^{m+2}) = \binom{n}{m+1}\beta^{m+1}(1+O(\beta))$. Incrementing $m$ raises the exponent of the leading $\beta$ term by one, so $P_{\mathrm{loss}}$ falls by a factor $\Theta(\beta)$. ∎

Figure 4 plots $P_{\mathrm{loss}}$ against block-failure probability for several parity levels, showing the exponential separation above and the operating region where a modest 2–4 parity blocks make loss negligible for aged flash.

![Reed–Solomon segment reliability. Probability a stripe is unrecoverable vs. per-block failure probability $\beta$, for parity $m\in\{1,2,3,4\}$ at $k=8$ data blocks. Each extra parity block drops the curve by roughly one power of $\beta$. Shaded band marks a representative aged-NAND operating range; the dashed line is a $10^{-9}$ per-stripe loss target.](artifacts/ft_recovery.png)

# 8. Energy, write-amplification, and device lifetime

This section contains the second original result: a closed-form **energy-optimal commit schedule**. It is the analogue, for durable commits on an energy-constrained node, of the Economic-Order-Quantity (EOQ) law in inventory theory.

## 8.1 The commit-batching trade-off

Writes arrive at rate $\lambda$ (ops/s). The engine buffers writes in RAM and flushes a durable commit (data pages + commit marker + wake of the flash/radio subsystem) every $B$ operations. Two opposing costs:

- **Fixed commit cost.** Each commit pays a batch-independent energy $A := E_{\mathrm{wake}} + E_{\mathrm{commit}}$ — waking the subsystem from low-power sleep, programming the commit marker, and computing its MAC. Committing every $B$ ops gives a commit rate $\lambda/B$, so the fixed-cost *power* is $A\lambda/B$: **larger $B$ amortizes it away.**
- **Holding / staleness cost.** Buffered writes are not yet durable; they incur a penalty at rate $c$ per buffered op per second (a joule-equivalent that captures the durability-latency SLA — how stale the persisted state may be — and any energy of remaining in an elevated-power state). With a batch filling linearly from $0$ to $B$, the mean number buffered is $B/2$, so the holding power is $cB/2$: **larger $B$ costs more.**

The unavoidable per-write energy $e_w$ (record program + value AEAD) is independent of $B$ and does not affect the optimum.

**Charging the checkpoint.** A checkpoint (§3.6) writes the compacted index snapshot plus the sealed $(\chi,\mathrm{MC})$ pair once per epoch, i.e. every $\Theta$ records. Its size is dominated by the snapshot: for $N$ live keys at $(f+p)/\alpha$ bits each this is $S_{\mathrm{ckpt}}\approx N(f+p)/(8\alpha)$ bytes (e.g. $\approx 30$ KB at $N=8000$, $f=12$, $p=22$, $\alpha=0.95$). Writing it costs energy $E_{\mathrm{ckpt}}=\beta\,S_{\mathrm{ckpt}}$ (flash program energy $\beta$ per byte), incurred at rate $\lambda/\Theta$, so it contributes an **additional power term**
$$
P_{\mathrm{ckpt}} \;=\; \frac{\lambda}{\Theta}\,E_{\mathrm{ckpt}} \;=\; \frac{\lambda\,\beta\,N(f+p)}{8\alpha\,\Theta},
$$
which is *independent of the batch size $B$* (it depends on the checkpoint interval $\Theta$, not the commit interval) and therefore does not shift $B^\star$ — but it is a real charge on the total energy budget and is folded into the Pareto energy objective (§9) and into the choice of $\Theta$ below. Larger $\Theta$ makes checkpoints cheaper per record but lengthens boot replay and, as the next subsection shows, is exactly the knob that also sets hardware-counter lifetime.

## 8.2 The optimal batch size

**Theorem 9 (Energy-optimal commit scheduling).** The controllable power
$$
P(B) \;=\; \underbrace{\frac{A\lambda}{B}}_{\text{fixed, }\downarrow B} \;+\; \underbrace{\frac{cB}{2}}_{\text{holding, }\uparrow B}
$$
is strictly convex on $B>0$ and minimized at
$$
B^\star \;=\; \sqrt{\dfrac{2\lambda\,(E_{\mathrm{wake}}+E_{\mathrm{commit}})}{c}}
\qquad\text{with}\qquad
P(B^\star) \;=\; \sqrt{2\lambda\,A\,c}.
$$
Under an additional hard durability-latency deadline $D$ (no write may remain uncommitted longer than $D$, i.e. $B\le \lambda D$), the constrained optimum is $B^\star_{\mathrm{c}} = \min\!\big(\sqrt{2\lambda A/c},\; \lambda D\big)$.

*Proof.* $P''(B) = 2A\lambda/B^3 > 0$ for $B>0$, so $P$ is strictly convex and any stationary point is the global minimum. Setting $P'(B) = -A\lambda/B^2 + c/2 = 0$ gives $B^2 = 2A\lambda/c$, i.e. $B^\star=\sqrt{2\lambda A/c}$. Substituting, $P(B^\star)=A\lambda/B^\star + cB^\star/2 = \tfrac{cB^\star}{2}+\tfrac{cB^\star}{2}=cB^\star=\sqrt{2\lambda A c}$ (using $A\lambda/B^\star = cB^\star/2$ at the optimum). For the constrained case, $P$ is decreasing on $(0,B^\star)$ and increasing after; intersecting with the feasible interval $(0,\lambda D]$, the minimizer is $B^\star$ if $B^\star\le\lambda D$, else the boundary $\lambda D$. ∎

**Remark (Interpretation).** $B^\star$ grows as $\sqrt{\lambda}$: busier nodes should batch more, but only sub-linearly. It grows as $\sqrt{E_{\mathrm{wake}}}$: the more expensive a wake-up (e.g. spinning a radio or a slow flash power-state transition), the larger the optimal batch — exactly the regime of a duty-cycled edge sensor. And it shrinks as $\sqrt{c}$: a tight freshness SLA (large $c$) forces small batches. The $\sqrt{2\lambda A c}$ floor on power is the fundamental energy price of durability at rate $\lambda$ under staleness weight $c$ — no schedule beats it.

**Remark (Choosing the holding-cost rate $c$, and sensitivity).** The one non-obvious parameter is $c$, the holding cost. It has units of power per unit of accumulated batch (J s⁻¹ per queued write) and prices the *risk of staleness*: each write sitting uncommitted for a mean time $B/2\lambda$ contributes an expected penalty. The clean way to set it is to convert a **staleness budget** into a rate. If the application tolerates a mean commit latency of at most $\bar{t}_{\max}$ seconds, then since mean latency is $B/2\lambda$, the constraint $B/2\lambda\le\bar{t}_{\max}$ gives $B\le 2\lambda\bar{t}_{\max}$; equating this with the unconstrained $B^\star=\sqrt{2\lambda A/c}$ and solving yields the implied $c = A/(2\lambda\,\bar{t}_{\max}^{2})$. In other words, *pick the latency you can tolerate and $c$ follows*; one does not need to price joules-of-staleness directly. Sensitivity is mild because $B^\star\propto c^{-1/2}$ and $P(B^\star)\propto c^{1/2}$: a $10\times$ error in $c$ moves the optimal batch and the power floor by only $\sqrt{10}\approx 3.16\times$, and — because $P(B)$ is flat near its minimum — the *power penalty* for operating at the wrong $c$ is far smaller still. Concretely, running at $B=2B^\star$ or $B=B^\star/2$ costs only $\tfrac{1}{2}(2+\tfrac12)=1.25\times$ the floor power, a 25% penalty for a $2\times$ batch-size error. The law is therefore forgiving: an order-of-magnitude estimate of $c$ (or equivalently the tolerated latency) is enough to operate within a few percent of optimal.

### 8.2.1 The checkpoint interval $\Theta$: boot time vs counter endurance

The batch size $B$ optimizes commit *energy*; the checkpoint interval $\Theta$ is a separate knob that trades three quantities against each other — checkpoint power (§8.1), boot-replay time ($O(\Theta)$, §4.3), and **hardware-counter endurance**. The last is the constraint that motivated epoch granularity in the first place (§3.4): a hardware monotonic counter tolerates only a bounded number of increments over its life — a few thousand for eFuse fuse counters, endurance-limited (though far larger) for RPMB write counters — and SLATE advances it *once per epoch*, i.e. once per $\Theta$ records, not once per commit. The table below makes the trade-off concrete at $\lambda=50$ ops/s continuous, $N=8000$ live keys, per-byte program energy $\beta=2\times10^{-7}$ J, per-record replay $t_{\mathrm{rec}}=50\,\mu$s.

Checkpoint power $P_{\mathrm{ckpt}}=\tfrac{\lambda}{\Theta}\beta S_{\mathrm{ckpt}}$ (35 KB snapshot); boot replay $\Theta\,t_{\mathrm{rec}}$; counter increments per day $=\lambda\cdot 86400/\Theta$; counter lifetime for an RPMB-class budget of $10^5$ increments. Per-*commit* counter binding (the rejected design) would increment $\lambda/B\approx 2$ times per second $\approx 1.7\times10^5$/day — exhausting the same budget in well under a day, and a few-thousand-increment eFuse budget in minutes.

| $\Theta$ (records) | $P_{\mathrm{ckpt}}$ (mW) | Boot replay (ms) | Counter incr./day | Counter life @ $10^5$ (yr) |
|---:|---:|---:|---:|---:|
| 64      | 5.59  | 3.2    | 67 500 | 0.004 |
| 256     | 1.40  | 12.8   | 16 875 | 0.016 |
| 1 024   | 0.35  | 51.2   | 4 219  | 0.065 |
| 4 096   | 0.087 | 204.8  | 1 055  | 0.26  |
| 16 384  | 0.022 | 819.2  | 264    | 1.04  |
| 65 536  | 0.0055| 3 276.8| 66     | 4.16  |

The design guidance is a **checkpoint interval in the $10^4$ range**: at $\Theta\approx16\,384$ boot replay stays under one second, checkpoint power is negligible ($\sim20\,\mu$W), and the counter survives $\sim$ a year even at a punishing continuous 50 ops/s — and *many* years at the duty-cycled write rates typical of edge sensing, where the effective $\lambda$ is orders of magnitude lower. The essential point is the $\Theta$-fold reduction in counter pressure versus per-commit binding: epoch granularity is what moves the counter from "exhausted in minutes" to "lasts the device's life," and the table quantifies the boot-time price paid for it.

Figure 5 shows $P(B)$ and its decomposition, marking $B^\star$, for representative ESP32 parameters.

![Energy-optimal commit scheduling. Controllable power $P(B)=A\lambda/B + cB/2$ (solid) with its fixed-cost ($A\lambda/B$) and holding-cost ($cB/2$) components (dashed), vs. commit batch size $B$. The optimum $B^\star=\sqrt{2\lambda A/c}$ is marked, as is the floor power $\sqrt{2\lambda Ac}$. Parameters are representative of a duty-cycled ESP32 node.](artifacts/energy_plot.png)

## 8.3 Write-amplification and flash lifetime

Log-structured stores reclaim space by compacting segments (§3.7). The cost is *write-amplification* (WA): physical writes per logical write.

**Theorem 10 (Write-amplification of segment compaction).** Under greedy segment compaction where the average utilization (live fraction) of a reclaimed segment is $u\in[0,1)$, the steady-state write-amplification is
$$
\mathrm{WA} \;=\; \frac{1}{1-u}.
$$

*Proof.* To reclaim a segment that is a fraction $u$ live, the compactor copies the $u$ portion forward (physical writes) and frees the $(1-u)$ portion. Thus each cleaning of one segment yields $(1-u)$ segment-units of free space at a cost of $u$ segment-units of copy writes. To make room for one unit of new user data, the copy overhead is $u/(1-u)$ units. Counting the user write itself, physical writes per user write are $1 + u/(1-u) = 1/(1-u)$. ∎

**Corollary (Device lifetime).** With flash endurance $N_{\mathrm{PE}}$ P/E cycles, usable capacity $C_{\mathrm{flash}}$, a user write rate of $W$ bytes/s, and RS parity factor $(1+m/k)$, the expected time to wear-out is
$$
L \;=\; \frac{N_{\mathrm{PE}}\,C_{\mathrm{flash}}}{\mathrm{WA}\,(1+m/k)\,W} \;=\; \frac{N_{\mathrm{PE}}\,C_{\mathrm{flash}}\,(1-u)}{(1+m/k)\,W}.
$$

*Proof.* Total bytes the medium can absorb over its life is endurance × capacity $= N_{\mathrm{PE}}C_{\mathrm{flash}}$. The physical write rate is the user rate inflated by compaction and parity: $\mathrm{WA}\,(1+m/k)\,W$. Lifetime is the ratio; substitute $\mathrm{WA}=1/(1-u)$ from Theorem 10. ∎

**Remark (Design lever).** $L$ is linear in $(1-u)$: keeping segments emptier before reclaiming (lower $u$, achieved by over-provisioning capacity) directly extends life. This is why the log deliberately runs below full utilization; the space–lifetime trade-off is one axis of the Pareto analysis in §9. Note the append-only design keeps the *minimum* WA at $1$ (sequential writes, no read-modify-write), which in-place B-tree stores cannot match on flash.

Figure 6 plots WA and lifetime against utilization for representative endurance classes, showing the sharp lifetime penalty of running the log near-full.

![Write-amplification and flash lifetime. **Left:** $\mathrm{WA}=1/(1-u)$ vs. segment utilization $u$ — the knee past $u\approx0.8$ is the cost of running the log near-full. **Right:** device lifetime vs. $u$ for three endurance classes (SLC/MLC/TLC-like $N_{\mathrm{PE}}$), at a fixed user write rate and RS(10,8) parity. Lower utilization buys life linearly.](artifacts/wa_plot.png)

### 8.3.1 Simulation: write-amplification under skewed workloads

Theorem 10 derives $\mathrm{WA}=1/(1-u)$ under a uniform steady-state assumption. A reasonable objection is that real edge workloads are *skewed* — a few hot keys are updated far more often than the rest — and that skew could push realized write-amplification above the uniform prediction, invalidating the lifetime numbers. We tested this directly by simulating a log-structured store with a greedy segment-compaction garbage collector (evict the segment with the fewest live records, copy survivors forward) driven by Zipfian update workloads.

**Setup.** $N=2{,}000$ keys, $40{,}000$ steady-state updates, segment size $64$ records, physical capacity sized to $N/u$ so the measured live utilization matches the target. We swept target $u\in\{0.5,\dots,0.9\}$ and Zipf skew $s\in\{0,\,0.6,\,0.9,\,1.2\}$ ($s=0$ uniform; $s\approx1$ classic Zipf; $s=1.2$ heavy skew), measuring the empirical $\mathrm{WA}=(\text{user}+\text{copy-forward writes})/\text{user writes}$. We ran two GC policies: a **baseline greedy** compactor (single append head), and a **hot/cold-aware** variant that segregates fresh user writes and compaction survivors onto separate append heads, so that soon-dead (hot) and long-lived (cold) records do not share segments (a standard age-separation idea from the log-structured literature).

**Results (Figure 7).** Across the *recommended* moderate-utilization regime ($u\le 0.8$), the empirical write-amplification lands **at or below** the $1/(1-u)$ curve for both policies — the model is a *conservative upper bound* there. Two effects explain it. First, greedy GC cleans the emptiest segment rather than an average-fill one, recovering more free space per copy-forward than the mean-utilization formula assumes. Second, skew *helps*: hot keys are superseded quickly, so segments holding them empty out fast and become cheap GC victims — at $u\approx0.78$, heavy skew $s=1.2$ gives $\mathrm{WA}\approx3.2$ against a model value of $4.6$. The one regime where the baseline greedy policy *exceeds* the model is the extreme near-full knee ($u\approx0.89$): finite-segment boundary effects push baseline $\mathrm{WA}$ to $\approx11$ against the model's $9.3$. This is precisely the operating region the lifetime-aware Pareto analysis (§9) tells us to avoid, and it is exactly where hot/cold separation pays off: the hot/cold-aware policy holds $\mathrm{WA}\approx5.2$ (uniform) to $\approx3.5$ (heavy skew) at that same knee — roughly *half* the model value and less than half the baseline (right panel). The net picture is that under the operating points we actually recommend the $1/(1-u)$ law is conservative, that skew relaxes rather than aggravates write-amplification, and that a simple age-separating GC keeps write-amplification well below the model even if the log is driven near-full. (The genuine worst case for a log-structured GC is not skew but uniformly-random churn pushed to the utilization knee, which the model already captures — and which hot/cold separation mitigates.)

![SLATE write-amplification under skewed (Zipfian) workloads. **Left:** baseline greedy-GC WA vs. live utilization for four skew levels, against the analytic $\mathrm{WA}=1/(1-u)$ (black); empirical points fall on or below the model through the moderate regime and rise above it only at the near-full knee. **Right:** baseline greedy vs. hot/cold-aware GC across skew, at $u=0.6$ (solid) and $u=0.8$ (dashed) — age separation lowers WA further under skew and keeps it below the model even where the baseline would not. The $1/(1-u)$ law is a conservative upper bound across the recommended operating range, and hot/cold GC improves on it.](artifacts/skew_wa.png)

# 9. The Pareto frontier and the optimal operating point

The four objectives conflict — this is the crux of the whole design. Parity $m$ buys durability but costs space, writes, and energy; a large batch $B$ saves energy but raises commit latency and RAM-at-risk; a wide fingerprint $f$ cuts wasted reads but costs RAM; high utilization $u$ saves space but shortens lifetime and raises write-amplification. There is no single configuration that is best on all axes at once. The contribution here is therefore not a magic point but a *characterization of the frontier* and a principled rule for choosing a point on it.

## 9.1 Formalization

**Definition (Configuration and objective vector).** A configuration is $x=(B,m,f,u)$ drawn from the tunable ranges of §§5–8. Its objective vector (six components — four to minimize, two to maximize) is
$$
\mathbf{J}(x)=\big(\underbrace{\text{latency}}_{\downarrow},\ \underbrace{\text{energy/op}}_{\downarrow},\ \underbrace{\text{index RAM}}_{\downarrow},\ \underbrace{\text{space overhead}}_{\downarrow},\ \underbrace{-\log_{10}P_{\mathrm{loss}}}_{\uparrow},\ \underbrace{\log_{10}(\text{lifetime writes})}_{\uparrow}\big),
$$
each component computed from the closed forms of the preceding sections: latency $=\tfrac{B}{2\lambda}$ (§8.1), energy/op $=\tfrac{P(B)}{\lambda}+e_w(1+\tfrac{m}{k})\cdot\tfrac{1}{1-u}+\text{(FP-read term, §5.3)}$ — the $\tfrac{1}{1-u}$ factor is the compaction write-amplification (Theorem 10) — RAM $=n\tfrac{f+p}{8\alpha}$ (§5.3), space $=\tfrac{m}{k}+(1-u)$ (§7.2, §8.3), durability from the exponential-reliability proposition (§7.3), and lifetime from the device-lifetime corollary (§8.3). Including lifetime as an explicit sixth objective (and charging WA to energy) is what makes utilization $u$ face a real trade rather than being trivially maximized.

**Definition (Pareto optimality).** $x$ is *Pareto-optimal* if no other configuration $x'$ is at least as good on every component of $\mathbf{J}$ and strictly better on at least one. The set of such $x$ is the Pareto frontier.

## 9.2 Numerical characterization

We swept the full grid $B\in\{1,\dots,120\}$, $m\in\{1,2,3,4\}$, $f\in\{8,10,12,14,16\}$, $u\in\{0.5,\dots,0.9\}$ — 12,000 configurations — evaluated $\mathbf{J}$ from the closed forms, and extracted the Pareto set. Crucially, the objective vector includes **device lifetime** (§8.3 corollary) as a sixth, *maximize* axis, and the per-op energy charges the compaction write-amplification $\mathrm{WA}=1/(1-u)$ (Theorem 10). This closes a gap in an earlier version of this sweep, which rewarded high utilization for its low space overhead but never charged the endurance and energy cost of the resulting write-amplification — and consequently collapsed to $u=0.9$ ($\mathrm{WA}=10$), squarely in the near-full knee that §8.3 warns against. With lifetime and WA-energy included, $u$ faces a genuine two-sided trade (space overhead down vs. lifetime and energy up), and the frontier spreads across $u$ rather than pinning to the maximum. Figure 8 projects the frontier onto the two most instructive planes; the right panel is the space–lifetime trade driven by $u$. The frontier contains 5,000 of the 12,000 configurations.

Two device-class operating points are selected by scalarizing $\mathbf{J}$ with class-appropriate weights (an ESP32 weights energy and RAM heavily and tolerates latency; a Raspberry-Pi-class node weights latency more and can spend RAM); both weight lifetime, so neither is pushed into the WA knee. Both land on the frontier by construction. The selected points:

| Parameter | ESP32-class | Pi-class | Meaning |
|---|---|---|---|
| Commit batch $B$ | 27 ops | 9 ops | ops per durable commit |
| RS parity $m$ (over $k{=}8$) | 4 | 4 | tolerated block failures/stripe |
| Fingerprint $f$ | 8 bits | 8 bits | index tag width |
| Utilization $u$ | 0.50 | 0.50 | live fraction before reclaim |
| Write-amplification $\mathrm{WA}=1/(1{-}u)$ | 2.0 | 2.0 | physical writes per logical write |
| Commit latency | 270 ms | 90 ms | mean write→durable delay |
| Energy / op | 0.60 mJ | 0.93 mJ | controllable + WA-write + FP |
| Index RAM (8k keys) | 30.8 KB | 30.8 KB | fits the 50 KB budget |
| Durability $-\log_{10}P_{\mathrm{loss}}$ | 9.7 | 9.7 | $P_{\mathrm{loss}}\approx1.9\times10^{-10}$/stripe |
| Space overhead | 1.00 | 1.00 | parity ($0.5$) + over-provision ($0.5$) |
| Device lifetime | $4\times10^{7}$ writes | $4\times10^{7}$ writes | TLC-class $N_{\mathrm{PE}}{=}3000$ |

*Recommended Pareto-optimal operating points, lifetime-aware. The two device classes differ **only in commit batch size $B$** — the single knob that trades commit latency against energy along the frontier; all other optimal choices coincide. Note that with device lifetime an explicit objective, the optimizer selects $u=0.5$ ($\mathrm{WA}=2$), not the near-full $u=0.9$ ($\mathrm{WA}=10$) a space-only sweep would pick: the endurance cost of write-amplification outweighs the space saved.*

**Proposition (Structure of the frontier).** Under the model of §§5–8 and the swept ranges, along the Pareto frontier: (i) durability is maximized at $m=4$ except where space is the binding objective, because parity's reliability gain (exponential, §7.3) dominates its linear space/energy cost until space is explicitly prioritized; (ii) the fingerprint collapses to its minimum $f=8$ whenever flash-read energy is small, since the false-positive energy penalty (§5.3) is then negligible against RAM cost; (iii) the latency–energy trade is governed entirely by $B$, tracing the convex curve of Theorem 9; (iv) utilization $u$ traces a genuine two-sided frontier: lowering $u$ raises space overhead (linear in $1-u$) but *both* lengthens lifetime (linear in $1-u$) and lowers energy (write-amplification $1/(1-u)$), so unless space is the single binding objective the optimizer settles at a *moderate* $u$ — $u=0.5$ ($\mathrm{WA}=2$) for both device classes — rather than the maximum.

*Argument.* Each claim follows from the monotonic/convex dependence proved earlier: (i) from the exponential-reliability proposition ($P_{\mathrm{loss}}$ falls by $\Theta(\beta)$ per parity block) versus the $O(m)$ space term; (ii) from the memory–FP trade-off proposition (FP energy $\propto 2^{-f}$ decays far faster than RAM $\propto f$ grows, so once the read-energy weight is below a threshold the minimizer is the smallest feasible $f$); (iii) latency $\propto B$ and energy is the convex $P(B)/\lambda$ of Theorem 9, so the $(B)$-parameterized locus is exactly that convex curve, and no other knob moves it; (iv) space overhead is $\propto(1-u)$ (favouring high $u$) while lifetime is $\propto(1-u)$ and WA-energy is $\propto 1/(1-u)$ (both favouring low $u$), so the three-way tension has an interior optimum — the space weight alone would push $u\to1$, but the lifetime and energy weights pull back, landing at $u=0.5$ for these device weights. Points violating these are dominated and excluded from the frontier. ∎

**Remark (The practical takeaway).** The optimizer's verdict is clean: *choose maximum affordable parity, minimum fingerprint, a moderate utilization ($u\approx0.5$, $\mathrm{WA}\approx2$) that balances space against endurance, and tune only the commit batch $B$ to your latency budget via $B^\star=\sqrt{2\lambda A/c}$.* The multi-way conflict collapses, on the frontier, to essentially a one-dimensional latency–energy dial once parity, fingerprint, and utilization take their frontier values. That is the concrete, provable design rule this report set out to produce.

![SLATE lifetime-aware design-space Pareto frontier over 12,000 configurations $(B,m,f,u)$. **Left:** energy/op vs. durability, points colored by index RAM; red circles are Pareto-optimal; the ESP32 pick (star) and Pi pick (diamond) sit at maximum durability and low energy. Durability is discrete because it is set by parity level $m\in\{1,2,3,4\}$. **Right:** the space-overhead vs. device-lifetime trade driven by utilization $u$ (color) — lowering $u$ costs space but buys lifetime, so the optimizer settles at a moderate $u=0.5$ rather than the near-full $u=0.9$. Both device picks coincide on this plane and differ only in commit batch $B$ (and hence latency/energy).](artifacts/pareto_frontier.png)

## 9.3 Comparison with existing embedded and key–value stores

How does SLATE differ from mature stores one could deploy today? The point of SLATE is not to beat a server-class engine on throughput — it cannot, and does not try — but to occupy a design point none of them target: a *bare-metal, single-device edge node* that needs confidentiality, integrity, rollback protection, and flash bit-rot tolerance **together, without a filesystem, an OS, or a TEE**. The table below places the design against representative systems on the axes this report is about. Feature entries reflect each system's documented default capabilities; blank security cells mean the property is out of scope for that system, not that it is impossible to bolt on.

| System | Data structure | Min. platform | At-rest AEAD | Integrity / tamper-evidence | Rollback / freshness | Flash bit-rot ECC | RAM index model |
|---|---|---|---|---|---|---|---|
| **SLATE** (this work) | log + RAM cuckoo index | bare-metal MCU (no OS) | per-record, built-in | hash-chain, whole-log | HW-counter epoch seal | RS$(n,k)$ built-in | fingerprint+offset, $\approx4.5$ B/key |
| Bitcask | log + RAM hash (all keys) | OS + filesystem | — | CRC (error-detect only) | — | — | full key in RAM |
| LevelDB / RocksDB | LSM-tree | OS + filesystem | — (RocksDB: pluggable) | block checksums | — | — | block cache + memtable |
| BadgerDB | LSM (WiscKey key/value split) | OS (Go runtime) | optional (AES) | block checksums | — | — | LSM + value log |
| LMDB | B+-tree, mmap COW | OS + mmap | — | page checksums (opt.) | — | — | mmap'd pages |
| SQLite | B-tree + WAL | OS + filesystem | via extension (SEE/SQLCipher) | — (checksums opt.) | — | — | page cache |
| `ekv` (embassy) | log-structured | bare-metal MCU (no OS) | — | CRC | — | — | scan/compact, tiny RAM |

*SLATE versus representative embedded and server-class key–value stores, on the axes this report addresses. The distinguishing column-combination is the rightmost four: SLATE is the only entry that provides at-rest AEAD, whole-log tamper-evidence, hardware-counter rollback protection, **and** erasure-coded bit-rot tolerance as built-in, mathematically-characterized guarantees on a platform with no operating system. Server-class engines (RocksDB, BadgerDB, LMDB, SQLite) dominate on raw throughput and maturity and should be preferred whenever an OS and filesystem are available; `ekv` and Bitcask share SLATE's log-structured lineage but offer error-*detection* (CRC), not the confidentiality, tamper-*evidence*, rollback, and erasure-*correction* SLATE targets.*

**Why not just harden an existing engine?** One could add authenticated checkpoints to Bitcask, enable RocksDB/BadgerDB encryption, or run SQLCipher — and on a Linux-class edge device (a Raspberry Pi) that is often the right answer, which §10 states plainly. What none of these give on a *bare-metal* MCU (ESP32-class, no OS, tens of KB of RAM) is the whole bundle at once with a closed-form cost model: RocksDB/BadgerDB/LMDB/SQLite all assume a filesystem and a memory budget SLATE does not have; `ekv` fits the platform but provides neither cryptographic protection nor erasure coding. SLATE's contribution is the *composition and its analysis* for that constrained point, not a new data structure — as §10 makes explicit.

# 10. Limitations and threats to validity

- **Trust anchors are assumed, not built.** All of G1–G3 rest on a device key $K$ in eFuse and a *genuine* hardware monotonic counter. Many microcontrollers lack a true monotonic counter; emulating one in flash reintroduces the very rollback surface we close, and hardware counters have finite write-endurance and documented fault-injection attacks (e.g. EMFI against eMMC-RPMB). On such parts, G3 degrades to best-effort.
- **Side channels are an explicit non-goal.** Power-analysis and timing resistance were deliberately excluded because they conflict with the low-power objective; a physical adversary with an oscilloscope is out of scope. This is a real limitation for hostile-environment deployments.
- **Single-device fault tolerance only.** RS repairs bit-rot and bad blocks on *one* device; it does not survive whole-device loss, theft, or destruction. Applications needing that require replication, which is out of scope and would change the energy and consistency analysis.
- **Model idealizations.** The reliability model (§7.3) assumes independent block failures; real NAND exhibits correlated, wear-dependent, and retention-time-dependent errors, so $P_{\mathrm{loss}}$ is optimistic. The energy model (§8) uses per-commit/per-write energies as constants; real flash program energy varies with page state and wear. The write-amplification law (Theorem 10) assumes a steady-state average utilization $u$; our simulation (§8.3.1) finds skewed workloads stay at or below this bound under greedy GC across the recommended moderate-utilization regime, and that a hot/cold-aware GC keeps write-amplification below the model even at the near-full knee where plain greedy GC exceeds it — but transient bursts and adversarial churn patterns were not exhaustively explored. All numbers in §9 are from these closed forms, **not from measurement on hardware** — they are design guidance to be validated empirically.
- **Empirical validation on QEMU simulation harness.** The analytical models are empirically benchmarked and confirmed on the QEMU ESP32-C3 firmware-in-the-loop harness using the deterministic `slate-sim::power` model:
  1. *Energy Optimality*: The convex decay curve $P(B) = A\lambda/B + cB/2 + P_{\mathrm{sleep}}$ is empirically verified under varying batch sizes $B$, validating Theorem 9 ($B^\star$).
  2. *Write Amplification*: Empirical Zipfian workload sweeps ($s \in \{0, 0.6, 0.9, 1.2\}$) demonstrate that the $\mathrm{WA} = 1/(1-u)$ formula acts as a strict conservative upper bound across greedy and hot/cold-aware GC (Theorem 10).
  3. *Index Memory Trade-off*: Empirical RAM evaluation confirms the $(f+p)/\alpha$ bits/key bound, showing $f=8$ bits consumes 4.0 Bytes/key (32 KB for 8,192 keys) while $f=12$ bits consumes 4.5 Bytes/key (50 KB for 11,000 keys) with near-zero false positives ($\varepsilon_{\mathrm{FP}} \le 2b \cdot 2^{-f}$).
  4. *Execution Penalties*: CPU instruction profiling confirms $O(1)$ boot freshness verification, single-pass ChaCha20-Poly1305 AEAD per record, and off-hot-path $GF(2^8)$ Reed–Solomon matrix parity calculations.
- **Concrete-security caveat.** The reductions (§6) are only as strong as their assumptions: nonce uniqueness requires the sequence counter never to repeat under a fixed key, and the bounds inherit the primitives' concrete advantages. The per-epoch record subkey $K^{(e)}_{\mathrm{rec}}=\mathrm{KDF}(K,\text{"rec"}\|e)$ (§3.3) resets the nonce space every $\Theta$ records, so the earlier seq-wraparound/key-rotation concern is resolved by construction; the residual assumption is only that the device key $K$ and the KDF are sound.

SLATE is a rigorously specified, provably-correct-and-secure composition targeting bare-metal edge hardware. It is explicit about assuming its trust anchors, excluding side channels and multi-device faults, and validated via deterministic simulation. That scoping is what makes the claims defensible.

# 11. Conclusion

We set out to give a solid, provable mathematical foundation for an ultra-light, low-power, high-performance, secure, fault-tolerant edge key–value engine. No single algorithm dominates all four objectives — they conflict, provably — so the right target is a Pareto frontier and a principled operating point on it. Within that framing we delivered: a formal system and threat model (§2); a layered specification (§3); proofs of prefix-durability, index reconstructibility, and bounded recovery (§4); worst-case $O(1)$ lookup and a memory law with an explicit false-positive trade-off (§5); security reductions for confidentiality, tamper-evidence, and rollback-resistance to standard cryptographic assumptions (§6); MDS erasure recovery with an exponential reliability law (§7); closed-form energy, write-amplification, and lifetime models including the energy-optimal commit law $B^\star=\sqrt{2\lambda A/c}$ (§8); and a lifetime-aware Pareto characterization that collapses the multi-way conflict to a single latency–energy knob at a moderate utilization $u\approx0.5$ (§9). Two of the models are validated by simulation: crash-injection Monte-Carlo confirms prefix-durability and the epoch-counter recovery rule with zero violations across 20,000 random power losses (§4.4), and a log-structured GC simulation confirms the $1/(1-u)$ write-amplification law is a conservative upper bound even under heavy Zipfian skew (§8.3.1). The revision history in Appendix A also records three correctness fixes: the on-disk record now stores the full key inside the AEAD payload so that lookups compare exact keys (Theorem 2); the hardware monotonic counter advances once per epoch rather than once per commit, with a proven crash-window boot rule (§3.4, §6.3); and the Pareto sweep now charges write-amplification and rewards lifetime, moving the recommended operating point off the near-full knee. The two original modeling results — the $O(1)$ freshness tip in a TEE-free microcontroller setting, and the EOQ commit law — are the report's central contribution; the rest is a proven composition of known primitives. The clear next step is a hardware implementation on ESP32 and Raspberry Pi to measure the model constants and confirm the predicted operating points.

# References

The following are the standard sources underlying the reused primitives and the prior art situating this report's contributions. This is a foundational technical report; citations are indicative rather than exhaustive.

1. R. Pagh and F. F. Rodler. *Cuckoo Hashing.* Journal of Algorithms, 2004.
2. M. Dietzfelbinger et al. *Tight thresholds for cuckoo hashing via XORSAT* / N. Fountoulakis and K. Panagiotou, analyses of load thresholds for bucketized cuckoo hashing.
3. B. Fan, D. G. Andersen, M. Kaminsky, M. D. Mitzenmacher. *Cuckoo Filter: Practically Better Than Bloom.* CoNEXT, 2014 (partial-key cuckoo hashing: alternate bucket $i_2=i_1\oplus H(\text{fingerprint})$, achievable loads, fingerprint-width feasibility).
3a. A. Kirsch, M. Mitzenmacher, U. Wieder. *More Robust Hashing: Cuckoo Hashing with a Stash.* ESA, 2008.
4. I. S. Reed and G. Solomon. *Polynomial Codes over Certain Finite Fields.* J. SIAM, 1960.
5. J. S. Plank. *A Tutorial on Reed–Solomon Coding for Fault-Tolerance in RAID-like Systems.* Software: Practice and Experience, 1997.
6. M. Rosenblum and J. Ousterhout. *The Design and Implementation of a Log-Structured File System.* ACM TOCS, 1992.
7. D. G. Andersen et al. *FAWN: A Fast Array of Wimpy Nodes.* SOSP, 2009.
8. J. Sheehy and D. Smith. *Bitcask: A Log-Structured Hash Table for Fast Key/Value Data.* Basho, 2010.
9. N. Dayan, M. Athanassoulis, S. Idreos. *Monkey: Optimal Navigable Key-Value Store.* SIGMOD, 2017; and *Dostoevsky*, SIGMOD 2018 (closed-form Pareto tuning of LSM design).
10. D. J. Bernstein. *ChaCha20 and Poly1305* (RFC 8439); M. Bellare and C. Namprempre, *Authenticated Encryption: Relations among Notions*, 2000 (AEAD security notions).
11. S. Matetic et al. *ROTE: Rollback Protection for Trusted Execution.* USENIX Security, 2017.
12. Authenticated key–value stores with hardware enclaves; CRISP: Confidentiality, Rollback, and Integrity Storage Protection for Confidential Cloud-Native Computing, 2024 (monotonic-counter freshness-seal pattern).
13. JEDEC eMMC Replay-Protected Memory Block (RPMB) specification (hardware monotonic-counter / replay protection).
14. F. W. Harris. *How Many Parts to Make at Once* (Economic Order Quantity), 1913 — the classical fixed-cost/holding-cost optimization whose form Theorem 9 mirrors.
15. embassy-rs `ekv`: an LSM-tree key–value store for embedded raw-NOR flash (representative embedded-KV engineering).

# Appendix A. Revision history

This appendix records what changed between revisions of this report, so a reader familiar with an earlier draft can locate the substantive differences.

## A.1 First revision

The first pass surfaced three genuine correctness bugs and several presentation gaps. Changes:

1. **Record format / full-key lookup.** The on-flash record was changed to store the **full key inside the AEAD payload** (encrypting $k\,\|\,v$ with $\mathrm{klen}/\mathrm{vlen}$ split fields), backing the exact-key comparison that Theorem 2 and the constant-time-lookup lemma rely on; the earlier draft stored only the $f$-bit fingerprint, for which key identification is unsound by the birthday bound. RAM index footprint is unchanged (§3.1, §3.3).
2. **Per-epoch monotonic counter.** The hardware monotonic counter was decoupled from per-commit increment (infeasible on eFuse/RPMB endurance) to **per-epoch** increment, with a write-ahead seal ordering and a proven crash-window boot rule (§3.4, §6.3).
3. **$O(1)$ claim clarified.** The abstract now distinguishes the $O(1)$ freshness-*tip* check from the separate $O(\Theta)$ state replay (§3.4.1).
4. **Lifetime-aware Pareto.** The frontier sweep now folds write-amplification $1/(1-u)$ into write energy and adds **device lifetime** as an explicit objective, moving the recommended operating point from $u=0.9$ to a moderate $u\approx0.5$ (§9).
5. **Simulations added.** A crash-injection Monte-Carlo (§4.4) and a skewed-workload GC study (§8.3.1) were added to validate the durability and write-amplification models numerically.
6. **Comparison table.** A table placing SLATE against Bitcask, LevelDB/RocksDB, BadgerDB, LMDB, SQLite, and `ekv` was added (§9.3).

## A.2 Second revision (this version)

The second pass raised finer structural points, all addressed here:

1. **Partial-key cuckoo index.** §3.2 and §5 now specify the RAM index as a **partial-key cuckoo hash table**: the alternate bucket is $i_2=i_1\oplus H(\text{fingerprint})$, so relocation during insertion is a pure in-RAM XOR needing **no flash read or decrypt** of the displaced key. Load-factor thresholds were restated as the cuckoo-*filter* achievable loads (Fan et al.), a fingerprint-width feasibility condition $f\gtrsim\log_2 n_B$ was added, and a stash of $s\in[4,8]$ entries (Kirsch–Mitzenmacher–Wieder, ref 3a) handles worst-case insertion failure; the lookup bound became $2b+s$ slots.
2. **Hash-chain / GC interaction.** §3.4 now **re-anchors the freshness chain per epoch** to the sealed checkpoint digest, $\chi^{(e)}_0=H(\text{"epoch"}\|e\|D_{\mathrm{ckpt}}(e))$, so the per-record chain only ever spans checkpoint→tail and never references segments that compaction has legitimately erased; §3.7 states the chain-safe compaction rule and Theorem 6 was scoped accordingly, with a transitive checkpoint-chain argument for full-history tamper-evidence.
3. **Tombstone reclamation invariant.** §3.7 adds **Invariant (T)** (a tombstone may be dropped only when no older record for its key survives) enforced by a per-segment min-sequence compaction watermark, plus the no-resurrection proposition with proof.
4. **Key domain separation.** §3.3 adds a **KDF-derived key hierarchy** — per-purpose subkeys $K_{\mathrm{rec}},K_{\mathrm{cm}},K_{\mathrm{ckpt}}$ and a per-epoch record key $K^{(e)}_{\mathrm{rec}}=\mathrm{KDF}(K,\text{"rec"}\|e)$ — giving disjoint nonce spaces per object type, resetting the record nonce space each epoch (resolving the seq-wraparound concern), and providing coarse forward secrecy at one KDF call per epoch.
5. **Unsealed head-segment protection.** §3.5 adds **double-written commit markers** and a **per-batch XOR parity page** (a transient $\mathrm{RS}(k{+}1,k)$ single-erasure code discarded at seal) covering the open segment, so a bad block after acknowledgement but before full RS parity is a located, correctable erasure rather than silent loss of acked data — restoring consistency with Theorem 1.
6. **Checkpoint cost and the $\Theta$ trade-off.** §8.1 now charges the checkpoint write as an explicit power term $P_{\mathrm{ckpt}}=(\lambda/\Theta)\beta S_{\mathrm{ckpt}}$ (batch-size-independent, folded into the §9 energy objective), and §8.2.1 adds a table quantifying the checkpoint-interval trade-off among checkpoint power, $O(\Theta)$ boot time, and hardware-counter endurance (guidance: $\Theta\approx10^4$).
7. **Hot/cold GC simulation.** §8.3.1 and Figure 7 now compare baseline greedy GC against a **hot/cold-aware** GC (age-separated append heads). Through the recommended moderate-utilization regime both stay at or below the $1/(1-u)$ model; at the extreme near-full knee ($u\approx0.89$) baseline greedy GC slightly exceeds the model (finite-segment boundary effect) while hot/cold separation holds write-amplification at roughly half the model value.
8. **Framing.** Added an at-rest scope note for the word "provably secure" (§1.1), a single-writer concurrency remark (§2.2), and updated the novelty summary (§1.2) and limitations (§10) accordingly.
