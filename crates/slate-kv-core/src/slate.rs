//! slate
#![allow(missing_docs)]

use crate::config::*;
use crate::epoch::EngineState;
use crate::error::Error;
use crate::gc::{SegState, SegTable};
use crate::index::Index;
use crate::log::{Log, Sealer};

/// Workspace for record staging and cryptography scratch space to avoid stack allocations.
pub struct ScratchWorkspace {
    /// Buffer for reading/staging records in gc/compaction.
    pub gc_rec_bytes:
        [u8; crate::config::REC_OVERHEAD + crate::config::MAX_KEY_LEN + crate::config::MAX_VAL_LEN],
    /// Scratch buffer for gc/compaction decryption.
    pub gc_scratch: [u8; crate::config::MAX_KEY_LEN + crate::config::MAX_VAL_LEN],
    /// Buffer for reading candidate records during index collision check.
    pub cand_rec_bytes:
        [u8; crate::config::REC_OVERHEAD + crate::config::MAX_KEY_LEN + crate::config::MAX_VAL_LEN],
    /// Scratch buffer for candidate record decryption.
    pub cand_scratch: [u8; crate::config::MAX_KEY_LEN + crate::config::MAX_VAL_LEN],
    /// Page buffer for checkpoint programming.
    pub page_buf: [u8; crate::config::MAX_PAGE_SIZE],
}

impl ScratchWorkspace {
    /// Creates a new scratch workspace with zeroed arrays.
    pub const fn new() -> Self {
        Self {
            gc_rec_bytes: [0u8; crate::config::REC_OVERHEAD
                + crate::config::MAX_KEY_LEN
                + crate::config::MAX_VAL_LEN],
            gc_scratch: [0u8; crate::config::MAX_KEY_LEN + crate::config::MAX_VAL_LEN],
            cand_rec_bytes: [0u8; crate::config::REC_OVERHEAD
                + crate::config::MAX_KEY_LEN
                + crate::config::MAX_VAL_LEN],
            cand_scratch: [0u8; crate::config::MAX_KEY_LEN + crate::config::MAX_VAL_LEN],
            page_buf: [0u8; crate::config::MAX_PAGE_SIZE],
        }
    }
}

impl Default for ScratchWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Slate<'a, F, C, S> {
    pub flash: F,
    pub counter: C,
    pub sealer: S,
    pub engine: EngineState,
    pub log_hot: Log<'a, F>,
    pub log_cold: Log<'a, F>,
    pub index: Index<'a>,
    pub segs: SegTable,
    pub ckpt_seg_seq: u64,
    pub sched: crate::sched::Scheduler,
    pub metrics: crate::metrics::Metrics,
    pub ckpt_buf: &'a mut [u8],
    pub rng: crate::index::XorShift64,
    pub scratch_buf: ScratchWorkspace,
}

/// Share of scanned records that compaction may relocate before it gives up.
///
/// Above this the engine reports `FlashFull` rather than continuing to copy
/// live data it has nowhere to put. Set well clear of ordinary operation:
/// Theorem 19 puts GC amplification at 1/(1-u), so a segment utilization of
/// u = 0.90 already costs 10x and u = 0.95 costs 20x — expensive, but a
/// deliberate choice for someone who sized their volume that tightly. What this
/// stops is the runaway past u = 1, where the live set does not fit at all and
/// each pass copies everything while freeing nothing.
pub const GC_FUTILE_RELOCATION_PCT: u32 = 97;

impl<'a, F: slate_kv_hal::AsyncFlash, C: slate_kv_hal::AsyncMonotonicCounter, S: Sealer>
    Slate<'a, F, C, S>
{
    pub fn index_points_to(&self, key_candidates: &[&[u8]], offset: u32) -> bool {
        // Stub: check if index maps any candidate key to this offset
        for &k in key_candidates {
            let mut cbuf = crate::index::CandidateBuf::new();
            self.index.candidates(k, &mut cbuf);
            if cbuf.as_slice().contains(&offset) {
                return true;
            }
        }
        false
    }

    /// Inserts or updates `key`'s index entry to `new_off`, deduplicating by the
    /// *full* key so a `Put` to an existing key overwrites its slot instead of
    /// leaking a stale one. Candidate records are located in the hot batch, the
    /// cold batch, or on flash, then AEAD-opened to compare the exact key
    /// (fingerprints collide freely, §3.1/Thm 4.2). Returns `Err(IndexFull)` if
    /// the cuckoo table and stash cannot absorb a new key.
    pub async fn index_update_offset_async(
        &mut self,
        key: &[u8],
        new_off: u32,
    ) -> Result<(), Error> {
        let mut cbuf = crate::index::CandidateBuf::new();
        self.index.candidates(key, &mut cbuf);
        let mut matching_off = None;
        for &cand_off in cbuf.as_slice() {
            if cand_matches_key_async(
                &mut self.flash,
                &mut self.sealer,
                self.log_hot.head.write_offset,
                self.log_hot.batch.data(),
                self.log_cold.head.write_offset,
                self.log_cold.batch.data(),
                &mut self.scratch_buf.cand_rec_bytes,
                &mut self.scratch_buf.cand_scratch,
                cand_off,
                key,
            )
            .await
            {
                matching_off = Some(cand_off);
                break;
            }
        }
        self.index.upsert(key, new_off, &mut self.rng, |cand_off| {
            Some(cand_off) == matching_off
        })
    }

    /// Reads the current value of `key` into `out`, returning its length.
    ///
    /// Resolves the record wherever it currently lives — the uncommitted hot
    /// batch, the uncommitted cold batch, or flash — via the same
    /// [`read_candidate_async`] path the index dedup uses. A reader that goes
    /// straight to `flash.read(off, ..)` instead is wrong for any record whose
    /// batch has not been committed yet: the offset is a *future* flash address
    /// that still reads as erased `0xFF`, so a `put` immediately followed by a
    /// `get` reports "not found" while the record is sitting intact in RAM.
    ///
    /// Returns `None` when the key is absent or the newest record for it is a
    /// tombstone. Candidates are AEAD-opened and compared on the *full* key, so
    /// a fingerprint collision cannot return another key's value.
    pub async fn get_into_async(&mut self, key: &[u8], out: &mut [u8]) -> Option<usize> {
        let mut cbuf = crate::index::CandidateBuf::new();
        self.index.candidates(key, &mut cbuf);

        for &cand_off in cbuf.as_slice() {
            let mut hdr_bytes = [0u8; REC_HDR_LEN];
            let total = match read_candidate_async(
                &mut self.flash,
                self.log_hot.head.write_offset,
                self.log_hot.batch.data(),
                self.log_cold.head.write_offset,
                self.log_cold.batch.data(),
                cand_off,
                &mut hdr_bytes,
                &mut self.scratch_buf.cand_rec_bytes,
            )
            .await
            {
                Some(t) => t,
                None => continue,
            };
            let hdr = match crate::record::RecordHeader::decode(&hdr_bytes) {
                Ok(h) => h,
                Err(_) => continue,
            };
            if hdr.klen as usize != key.len() {
                continue;
            }
            if self
                .sealer
                .open_record(
                    &hdr_bytes,
                    &self.scratch_buf.cand_rec_bytes[REC_HDR_LEN..total],
                    &mut self.scratch_buf.cand_scratch,
                )
                .is_err()
            {
                continue;
            }
            let klen = hdr.klen as usize;
            if &self.scratch_buf.cand_scratch[..klen] != key {
                continue;
            }
            // A tombstone is a match, but the key has no value.
            if hdr.op == OP_DEL {
                return None;
            }
            let vlen = hdr.vlen as usize;
            if vlen > out.len() {
                return None;
            }
            out[..vlen].copy_from_slice(&self.scratch_buf.cand_scratch[klen..klen + vlen]);
            return Some(vlen);
        }
        None
    }

    /// Removes `key` from the index, matching the *full* key so a fingerprint
    /// collision cannot evict a different live key's slot. Returns whether an
    /// entry was removed.
    pub async fn index_remove_key_async(&mut self, key: &[u8]) -> bool {
        let mut cbuf = crate::index::CandidateBuf::new();
        self.index.candidates(key, &mut cbuf);
        let mut matching_off = None;
        for &cand_off in cbuf.as_slice() {
            if cand_matches_key_async(
                &mut self.flash,
                &mut self.sealer,
                self.log_hot.head.write_offset,
                self.log_hot.batch.data(),
                self.log_cold.head.write_offset,
                self.log_cold.batch.data(),
                &mut self.scratch_buf.cand_rec_bytes,
                &mut self.scratch_buf.cand_scratch,
                cand_off,
                key,
            )
            .await
            {
                matching_off = Some(cand_off);
                break;
            }
        }
        self.index
            .remove(key, |cand_off| Some(cand_off) == matching_off)
    }

    /// Appends one record to the hot log, advancing `next_seq` and the epoch
    /// record counter. Every production write MUST go through this (or
    /// [`Self::append_cold`]) rather than calling `Log::append` directly:
    /// `records_in_epoch` is what drives the Θ-triggered epoch seal in
    /// [`Self::commit`], and a write that bypasses the counter silently
    /// disables checkpointing, unbounded-mount protection and GC for the whole
    /// database.
    pub fn append_hot(&mut self, op: u8, key: &[u8], val: &[u8]) -> Result<u32, Error> {
        let seq = self.engine.next_seq;
        let (_seq_ret, offset) = self.log_hot.append(
            seq,
            self.engine.epoch,
            op,
            key,
            val,
            &mut self.sealer,
            &mut self.engine.chain,
        )?;
        self.engine.next_seq += 1;
        self.engine.records_in_epoch += 1;
        // Counted in the core append path so the no_std targets get write
        // amplification too; previously only slate-kv and slate-kv-sim counted
        // user bytes, leaving every on-device WA figure at zero-over-zero.
        self.metrics
            .add_user_bytes((REC_OVERHEAD + key.len() + val.len()) as u64);
        Ok(offset)
    }

    pub async fn append_cold_async(
        &mut self,
        key: &[u8],
        val: &[u8],
        now_ms: u64,
    ) -> Result<u32, Error> {
        let seq = self.engine.next_seq;
        let (_seq_ret, offset) = self.log_cold.append(
            seq,
            self.engine.epoch,
            OP_PUT,
            key,
            val,
            &mut self.sealer,
            &mut self.engine.chain,
        )?;
        self.engine.next_seq += 1;
        self.engine.records_in_epoch += 1;
        if self.sched.on_append(now_ms) {
            self.commit_async().await?;
        }
        Ok(offset)
    }

    pub async fn append_cold_tombstone_async(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<(), Error> {
        let seq = self.engine.next_seq;
        let _ = self.log_cold.append(
            seq,
            self.engine.epoch,
            OP_DEL,
            key,
            &[],
            &mut self.sealer,
            &mut self.engine.chain,
        )?;
        self.engine.next_seq += 1;
        self.engine.records_in_epoch += 1;
        if self.sched.on_append(now_ms) {
            self.commit_async().await?;
        }
        Ok(())
    }

    pub fn cold_batch_full(&self) -> bool {
        // Assume batch full if offset > limit
        self.log_cold.batch.data().len() >= 1024
    }

    /// Commits the pending batches, first making room for them.
    ///
    /// This is the entry point application code should use. It reserves space
    /// for the hot batch — relocating the head and reclaiming a segment when
    /// needed — and then programs. Skipping the reservation is what turns a full
    /// log into a permanent hard failure: the head advances past the last
    /// segment into unmanaged flash, `program` returns `OutOfBounds`, and every
    /// later commit reports a bare `Io`.
    pub async fn commit_async(&mut self) -> Result<(), Error> {
        self.commit_inner_async().await?;
        // Reserve *after* programming, never before. `Log::append` hands the
        // index an offset computed from the head at append time, so relocating
        // the head with records pending would leave every offset in the batch
        // pointing at the wrong address — a put would appear to succeed and then
        // read back as "not found". With the batch now empty, no offset is
        // outstanding and the head can move freely.
        self.reserve_space_async().await
    }

    /// Commits without reserving space.
    ///
    /// GC calls this rather than [`Self::commit_async`]: reclaiming space is
    /// what GC is doing, so re-entering the reservation path from inside it
    /// would make the future type infinitely sized (`commit → reserve →
    /// compact → commit`), which rustc rejects outright.
    pub(crate) async fn commit_inner_async(&mut self) -> Result<(), Error> {
        let seq_max = self.engine.next_seq.saturating_sub(1);

        // Bound both heads explicitly against the physical end of the region.
        // Without this the HAL reports the overrun as a bare `Io` (on the board:
        // `EspFlash program error: not erased at addr 2096640`, repeating
        // forever), which names neither the cause nor the fact that the volume
        // is simply full.
        let limit = self.flash.capacity();
        let hot_need = self.pending_hot_bytes();
        if hot_need > 0 && self.log_hot.head.write_offset.saturating_add(hot_need) > limit {
            return Err(Error::FlashFull);
        }
        let cold_need = self.pending_cold_bytes();
        if cold_need > 0 && self.log_cold.head.write_offset.saturating_add(cold_need) > limit {
            return Err(Error::FlashFull);
        }

        // A batch must also stay inside its segment's DATA area. Running past
        // it would write into the 4 parity blocks that `encode_parity` owns,
        // and would straddle the segment boundary — leaving a reclaimed
        // segment's first byte mid-ciphertext, which the compaction scan
        // cannot walk. Roll the head instead; this is the normal path once a
        // segment fills, not an error.
        if self.segs.num_segments > 0 {
            if hot_need > 0 && self.head_needs_roll(true, hot_need) {
                self.roll_head_async(true, hot_need).await?;
            }
            if cold_need > 0 && self.head_needs_roll(false, cold_need) {
                self.roll_head_async(false, cold_need).await?;
            }
        }

        let hot_bytes = self
            .log_hot
            .commit_async(
                &mut self.flash,
                &mut self.sealer,
                &self.engine.chain,
                self.engine.epoch,
                seq_max,
            )
            .await?;
        let cold_bytes = self
            .log_cold
            .commit_async(
                &mut self.flash,
                &mut self.sealer,
                &self.engine.chain,
                self.engine.epoch,
                seq_max,
            )
            .await?;

        // Parity pages and commit markers are flash traffic the application
        // never requested; counting them here is what makes the reported write
        // amplification differ from a hardcoded 1.0.
        self.metrics
            .add_parity_bytes(hot_bytes.parity_bytes + cold_bytes.parity_bytes);
        self.metrics
            .add_marker_bytes(hot_bytes.marker_bytes + cold_bytes.marker_bytes);
        // Data pages are whole pages; the part of them not covered by record
        // bytes is padding. Counting it here is what makes
        // user + gc + parity + marker + ckpt + padding = bytes programmed hold.
        let data_pages_total = hot_bytes.data_bytes + cold_bytes.data_bytes;
        let payload_total = hot_bytes.payload_bytes + cold_bytes.payload_bytes;
        self.metrics
            .add_padding_bytes(data_pages_total.saturating_sub(payload_total));

        self.engine.acked_seq = seq_max;

        // Advance the segment lifecycle to wherever the head now sits: seal the
        // segments the head has left behind and open the one it entered. GC
        // only ever picks a `Sealed` victim, so if this never runs the table
        // stays all-`Free`, `pick_victim` returns `None`, `compact_one` is a
        // no-op, and the head runs off the end of the region — every program
        // after that fails with a bare `Io`.
        let acked = self.engine.acked_seq;
        let hot_head = self.log_hot.head.write_offset;
        let hot_seq = self.log_hot.head.seg_seq;
        self.segs
            .open_at(hot_head, hot_seq, acked, SegState::OpenHot);
        let cold_head = self.log_cold.head.write_offset;
        let cold_seq = self.log_cold.head.seg_seq;
        self.segs
            .open_at(cold_head, cold_seq, acked, SegState::OpenCold);

        self.sched.on_commit();
        self.metrics.add_commit();
        // Each durable commit wakes the flash subsystem from low-power sleep;
        // this is the E_wake half of the fixed commit cost A (§8.1) and feeds the
        // energy model in slate-sim.
        self.metrics.add_wake();

        if self.engine.records_in_epoch >= crate::config::THETA {
            self.seal_epoch_now_async().await?;
        }
        Ok(())
    }

    /// Segments currently occupied by a log head. These must never be chosen as
    /// a GC victim: reclaim erases the whole segment.
    pub(crate) fn live_segments(&self) -> [u32; 2] {
        let hot = self
            .segs
            .seg_of(self.log_hot.head.write_offset)
            .unwrap_or(u32::MAX);
        let cold = self
            .segs
            .seg_of(self.log_cold.head.write_offset)
            .unwrap_or(u32::MAX);
        [hot, cold]
    }

    /// Bytes the pending hot batch will occupy on flash once committed: the
    /// batch pages, one XOR parity page, and two commit-marker pages.
    fn pending_hot_bytes(&self) -> u32 {
        Self::pending_bytes(self.flash.page_size() as u32, &self.log_hot)
    }

    /// Same accounting for the cold log, which is committed alongside the hot
    /// one on every commit and therefore needs the same bounds check.
    fn pending_cold_bytes(&self) -> u32 {
        Self::pending_bytes(self.flash.page_size() as u32, &self.log_cold)
    }

    /// Flash bytes to keep available ahead of a head after each commit.
    ///
    /// Sized from the scheduler's commit granularity (`b_commit` records at the
    /// maximum record size) plus the parity and marker pages, capped to a
    /// quarter segment. It deliberately does NOT use `batch.capacity()`: the
    /// host build allocates 64 KiB batch buffers, which exceed a single
    /// `SEG_BYTES` (49 152 B) segment, so a capacity-based reservation could
    /// never be satisfied and every commit would report `FlashFull`.
    /// Bytes a head should keep in reserve before rolling to a new segment.
    ///
    /// Sized from the largest record this instance has actually written, not
    /// from `MAX_KEY_LEN + MAX_VAL_LEN`. The worst-case record is 1,324 B, so a
    /// batch of 8 reserved 11,520 B — 35% of a segment's 32,768 B data area —
    /// and a head rolled with a third of the segment still erased. The ESP32-C3
    /// trace shows the cost directly: 14 commits per segment instead of 21,
    /// 9 segments consumed per 1,000 records instead of 6, and therefore ~50%
    /// more erases per record than the geometry requires. On a part rated for
    /// 100k cycles that is a third of the device's life spent on padding.
    ///
    /// Reserving too little is safe: `commit_inner_async` re-checks the exact
    /// pending byte count against the segment's data end and rolls there if the
    /// batch does not fit. This value only decides how early the roll happens,
    /// so the floor below is a smoothing term, not a correctness bound.
    fn reserve_headroom(&self) -> u32 {
        let page = self.flash.page_size() as u32;
        let seg = crate::config::SEG_BYTES as u32;

        // Largest record seen so far, falling back to a full page before the
        // first write. `max_record_bytes` is tracked on the log head, which is
        // always compiled — `Metrics` is behind an off-by-default feature.
        let rec = self.log_hot.head.max_record_bytes.max(page);
        let batch = rec.saturating_mul(self.sched.cfg.b_commit.max(1));
        let need = (batch.div_ceil(page) + 3) * page;

        // Never reserve more than an eighth of a segment: beyond that the
        // padding costs more than the occasional mid-commit roll it avoids.
        core::cmp::min(need, seg / 8)
    }

    fn pending_bytes(page: u32, log: &Log<'a, F>) -> u32 {
        let data = log.batch.data().len() as u32;
        if data == 0 {
            return 0;
        }
        (data.div_ceil(page) + 3) * page
    }

    /// True if `len` bytes at `off` are all erased (`0xFF`) and therefore
    /// programmable.
    async fn is_erased_at(&mut self, off: u32, len: u32) -> bool {
        if off.saturating_add(len) > self.flash.capacity() {
            return false;
        }
        let page = self.flash.page_size() as u32;
        let mut buf = [0u8; crate::config::MAX_PAGE_SIZE];
        let mut a = off;
        let stop = off + len;
        while a < stop {
            let n = core::cmp::min(page, stop - a) as usize;
            if self.flash.read(a, &mut buf[..n]).await.is_err() {
                return false;
            }
            if buf[..n].iter().any(|&b| b != crate::config::ERASED_BYTE) {
                return false;
            }
            a += n as u32;
        }
        true
    }

    /// Post-commit housekeeping: keep both log heads pointing at erased flash,
    /// and reclaim a segment when space is running low.
    ///
    /// **The head-is-erased invariant is the fix for the reported board
    /// failure.** The cold log head is initialized to `data_base` and only moves
    /// when the cold log is written, but the hot log starts at the same address
    /// and advances immediately. The first cold write — a GC relocation or a
    /// `del` tombstone — therefore programmed pages the hot log had already
    /// filled, which NOR flash rejects: on the C3 that surfaced as
    /// `EspFlash program error: not erased at addr 2096640` repeating forever,
    /// and on the host as `ProgramWithoutErase` at `data_base`.
    ///
    /// Runs with both batches empty (straight after a commit), so no index
    /// offset is outstanding and a head may be moved safely — moving a head
    /// affects only future appends; records already written stay where they are
    /// and the index keeps pointing at them.
    async fn reserve_space_async(&mut self) -> Result<(), Error> {
        if self.segs.num_segments == 0 {
            return Ok(());
        }
        let headroom = self.reserve_headroom();

        // 1. Top up the free-segment supply FIRST, so the rolls below have
        //    somewhere to go and so an ordinary commit never has to wait for a
        //    full compaction on its latency path.
        self.refill_free_segments_async().await?;

        // 2. Roll either head that cannot absorb another batch inside its
        //    current segment. This is the circular allocator: the head moves to
        //    a free segment, which may sit at a LOWER address than the one it
        //    is leaving. Before this existed the head could only advance, so a
        //    device halted with `FlashFull` after one linear pass while nearly
        //    every segment was free and erased — the defect all five reviewers
        //    raised as blocking.
        if self.head_needs_roll(true, headroom) {
            self.roll_head_async(true, headroom).await?;
        }
        if self.head_needs_roll(false, headroom) {
            self.roll_head_async(false, headroom).await?;
        }

        // 2. Hot and cold must never share a segment: reclaim erases a whole
        //    segment, so a shared one cannot be freed without destroying the
        //    other log's records.
        let hot_seg = self.segs.seg_of(self.log_hot.head.write_offset);
        let cold_seg = self.segs.seg_of(self.log_cold.head.write_offset);
        if hot_seg.is_some() && hot_seg == cold_seg {
            self.roll_head_async(false, headroom).await?;
        }

        // 3. Each head must point at erased flash. A head that has just rolled
        //    satisfies this by construction, but one restored from a checkpoint
        //    may not.
        let cold_off = self.log_cold.head.write_offset;
        if !self.is_erased_at(cold_off, headroom).await {
            self.roll_head_async(false, headroom).await?;
        }
        let hot_off = self.log_hot.head.write_offset;
        if !self.is_erased_at(hot_off, headroom).await {
            self.roll_head_async(true, headroom).await?;
        }

        // 5. Leave the supply topped up for the next commit.
        self.refill_free_segments_async().await
    }

    /// Opens `seg_id` for `state`, writing its on-flash header, and points the
    /// named head at the first byte above that header.
    ///
    /// Every segment the log enters goes through here. The header is what makes
    /// the log orderable by allocation number instead of by address, which is
    /// the precondition for wrapping: after a wrap the oldest live segment sits
    /// at a *higher* address than the newest, so an address-ordered recovery
    /// would replay the log backwards.
    async fn open_segment_async(&mut self, seg_id: u32, hot: bool) -> Result<(), Error> {
        let seg_base = self.segs.seg_base(seg_id);

        // The segment must be genuinely erased before we stamp a header on it:
        // programming over live data fails `ProgramWithoutErase` on NOR, and
        // succeeding would mean we had just handed out a segment holding
        // records the index still points at.
        let page = self.flash.page_size() as u32;
        if !self.is_erased_at(seg_base, page).await {
            return Err(Error::FlashFull);
        }

        let seg_seq = self.segs.next_seg_seq;
        let acked = self.engine.acked_seq;
        let epoch = self.engine.epoch;

        // The header MAC binds the header to this volume's key hierarchy, so a
        // header lifted from another device (or another epoch) does not
        // authenticate. `commit_marker` is reused as the MAC primitive rather
        // than adding a second one to the Sealer trait.
        let mac_src = self
            .sealer
            .commit_marker(seg_seq, epoch, 0, &self.engine.chain.chi);
        let mut hdr_mac = [0u8; 32];
        let n = core::cmp::min(32, mac_src.len());
        hdr_mac[..n].copy_from_slice(&mac_src[..n]);

        let first =
            crate::segment::write_header(&mut self.flash, seg_base, seg_seq, epoch, acked, hdr_mac)
                .await?;

        let state = if hot {
            SegState::OpenHot
        } else {
            SegState::OpenCold
        };
        self.segs.open_at(first, seg_seq, acked, state);

        if hot {
            self.log_hot.head.write_offset = first;
            self.log_hot.head.seg_seq = seg_seq;
            self.log_hot.head.block_idx = first / self.flash.block_size() as u32;
        } else {
            self.log_cold.head.write_offset = first;
            self.log_cold.head.seg_seq = seg_seq;
            self.log_cold.head.block_idx = first / self.flash.block_size() as u32;
        }
        Ok(())
    }

    /// Rolls a head into a fresh segment when its current one cannot hold
    /// `need` more bytes. This is the circular allocator.
    ///
    /// The head may move to a *lower* address than it currently occupies. That
    /// is the whole point: the region is a ring of segments, and the engine's
    /// lifetime is bounded by flash endurance rather than by the linear
    /// distance to the end of the partition. Before this existed, the head
    /// advanced monotonically and every device halted with `FlashFull` after a
    /// single pass while nearly all of its segments sat free and erased.
    ///
    /// When no segment is free, one reclaim is attempted before giving up, so
    /// `FlashFull` now means "the live set genuinely fills the volume" rather
    /// than "the head reached the end of the address space".
    /// Allocation only: takes a free segment if one exists, and never
    /// compacts.
    ///
    /// The no-compaction rule is a hard structural constraint, not a
    /// preference. This is reachable from `commit_inner_async`, and
    /// `compact_one_async` calls `commit_inner_async` back to flush relocated
    /// records — so a compacting roll here makes the future type infinitely
    /// sized (`commit → roll → compact → commit`) and rustc rejects the crate
    /// outright. Reclaim therefore happens in `reserve_space_async`, which runs
    /// after each commit and keeps free segments in reserve so this call has
    /// supply to draw on.
    async fn roll_head_async(&mut self, hot: bool, _need: u32) -> Result<(), Error> {
        // Program buffered records before the head moves: their index offsets
        // were computed against the head's current position and would otherwise
        // point into the wrong segment.
        self.flush_before_roll_async(hot).await?;

        let in_use = self.live_segments();
        match self.segs.pick_free_excluding(&in_use) {
            Some(id) => self.open_segment_async(id, hot).await,
            // Genuinely out of space: every segment is either live, or holds
            // data the newest checkpoint still depends on.
            None => Err(Error::FlashFull),
        }
    }

    /// Reclaims until at least one segment is free, or nothing more can be
    /// reclaimed.
    ///
    /// Only ever called from `reserve_space_async`, i.e. outside the commit
    /// future, so it may compact freely.
    async fn refill_free_segments_async(&mut self) -> Result<(), Error> {
        for _ in 0..2 {
            if self.segs.free_count() > 1 {
                return Ok(());
            }
            // Refuse to reclaim when reclaiming cannot help.
            //
            // Compaction frees a segment by relocating the live records out of
            // it. When the live set approaches the capacity of the log area
            // there is nowhere for them to go, so each pass copies almost
            // everything it reads and frees almost nothing — and because the
            // engine never says no, it does this forever. Measured on a 10
            // segment region with a live set 2.8x the log area: 321,871
            // relocations to place 8,000 keys (40 per key), 3,366 segment
            // reclaims (337 full passes of the region), 67,887 erases, and a
            // write amplification of 229 with checkpoint traffic at 120x the
            // user data. A device doing that has spent a meaningful fraction of
            // its flash endurance in minutes, while reporting success.
            //
            // This is the u -> 1 limit of Theorem 19 (WA_gc = 1/(1-u)) crossing
            // into the region where the theorem no longer applies: above u = 1
            // the data simply does not fit, and the honest answer is
            // `FlashFull`. The guard is deliberately conservative — it triggers
            // only once the live set exceeds `GC_FUTILE_UTILIZATION` of the log
            // area — so ordinary high-utilization operation still compacts.
            if self.gc_is_futile(GC_FUTILE_RELOCATION_PCT) {
                return Err(Error::FlashFull);
            }
            let in_use = self.live_segments();
            if self
                .segs
                .pick_victim_excluding(self.ckpt_seg_seq, &in_use)
                .is_none()
            {
                // Qualifying a victim costs a checkpoint (33 KiB and 9 erases at
                // default geometry) and cannot produce one when nothing is
                // sealed, so guard before paying for it. Unguarded, this fired
                // on every commit once space ran low: the pre-fix run burned 13
                // checkpoints in 112 records and pushed WA from 2.74 to 3.62
                // while reclaiming nothing.
                if self.segs.count_in_state(SegState::Sealed) == 0 {
                    return Ok(());
                }
                // A reclaim watermark needs a checkpoint, not a new epoch:
                // sealing here would spend a hardware monotonic-counter
                // increment on storage housekeeping.
                self.checkpoint_for_reclaim_async().await?;
            }
            let in_use = self.live_segments();
            match self
                .segs
                .pick_victim_excluding(self.ckpt_seg_seq, &in_use)
                .is_some()
            {
                true => crate::gc::compact_one_async(self).await?,
                false => return Ok(()),
            }
        }
        Ok(())
    }

    /// True when compaction is relocating nearly everything it reads, i.e. the
    /// live set no longer leaves room to work in.
    ///
    /// Measured from the most recent compaction rather than estimated from the
    /// index: `last_gc_relocated / last_gc_scanned` is the fraction of records
    /// that were still live when the last victim segment was walked. The
    /// counters live on `SegTable` rather than `Metrics` because `Metrics` sits
    /// behind an off-by-default feature, and a correctness guard must not
    /// depend on whether telemetry was compiled in. A healthy workload leaves
    /// most records dead by the time their segment is reclaimed, so this ratio
    /// stays low; as the live set approaches the capacity of the log area it
    /// climbs toward 1, and every pass copies everything and frees nothing.
    ///
    /// Only meaningful once enough segments have been walked to be
    /// representative, hence the sample floor.
    ///
    /// Integer arithmetic throughout — the crate denies floating point so the
    /// same code runs on cores without an FPU.
    fn gc_is_futile(&self, pct: u32) -> bool {
        // Enough records for the ratio to mean something. A segment holding
        // only a handful of large records can legitimately be all-live.
        const MIN_SAMPLE: u32 = 64;
        let scanned = self.segs.last_gc_scanned;
        if scanned < MIN_SAMPLE {
            return false;
        }
        self.segs.last_gc_relocated.saturating_mul(100) > scanned.saturating_mul(pct)
    }

    /// True when the head named by `hot` cannot absorb `need` more bytes inside
    /// its current segment.
    fn head_needs_roll(&self, hot: bool, need: u32) -> bool {
        if need == 0 || self.segs.num_segments == 0 {
            return false;
        }
        let off = if hot {
            self.log_hot.head.write_offset
        } else {
            self.log_cold.head.write_offset
        };
        // A head outside the managed area (a fresh volume, or one mounted from
        // a checkpoint written before segments were materialized) must roll
        // into a real segment before it can be used.
        !self.segs.fits_in_segment(off, need)
    }

    /// Moves the cold head to a segment where it can actually program: a free
    /// segment if one exists, otherwise the erased flash just past the hot head.
    async fn relocate_cold_head_async(&mut self, need: u32) -> Result<(), Error> {
        let hot_seg = self.segs.seg_of(self.log_hot.head.write_offset);

        // Prefer a free segment that the hot head does not occupy.
        let mut candidate = None;
        for i in 0..self.segs.num_segments {
            let e = &self.segs.entries[i as usize];
            if e.state == SegState::Free && Some(e.id) != hot_seg {
                candidate = Some(self.segs.seg_base(e.id));
                break;
            }
        }

        // Otherwise fall back to the next segment boundary above the hot head,
        // which is erased if the hot log has not reached it yet.
        if candidate.is_none() {
            if let Some(id) = hot_seg {
                let next = self.segs.seg_base(id + 1);
                if next + need <= self.flash.capacity() {
                    candidate = Some(next);
                }
            }
        }

        let dst = match candidate {
            Some(d) => d,
            None => return Err(Error::FlashFull),
        };
        if !self.is_erased_at(dst, need).await {
            return Err(Error::FlashFull);
        }

        self.log_cold.head.write_offset = dst;
        self.log_cold.head.block_idx = dst / self.flash.block_size() as u32;
        self.log_cold.head.seg_seq += 1;
        let seq = self.log_cold.head.seg_seq;
        let acked = self.engine.acked_seq;
        self.segs.open_at(dst, seq, acked, SegState::OpenCold);
        Ok(())
    }

    /// Programs any buffered records whose offsets a head roll would
    /// invalidate.
    ///
    /// `Log::append` hands back `head.write_offset + batch.offset` and the
    /// index stores that immediately, so every offset issued since the last
    /// commit is relative to the head's CURRENT position. Moving the head while
    /// the batch is non-empty silently relocates the bytes those offsets refer
    /// to: the probe found the index pointing at page 111 of a segment where
    /// only 6 pages had ever been programmed, and all 16 keys read back as
    /// `None` after a remount even though the checkpoint had restored them.
    ///
    /// Must be called before any head roll, not merely before a checkpoint.
    async fn flush_before_roll_async(&mut self, hot: bool) -> Result<(), Error> {
        let empty = if hot {
            self.log_hot.batch.is_empty()
        } else {
            self.log_cold.batch.is_empty()
        };
        if empty {
            return Ok(());
        }
        self.flush_pending_batches_async().await
    }

    /// Programs whatever is sitting in the hot and cold batch buffers.
    ///
    /// The write half of `commit_inner_async` with none of the space
    /// management. Checkpoint paths need exactly this: the index they are about
    /// to serialize refers to offsets that only become real once the batch is
    /// programmed, but they cannot call the full commit path because they are
    /// reachable from inside it.
    async fn flush_pending_batches_async(&mut self) -> Result<(), Error> {
        let epoch = self.engine.epoch;
        let seq_max = self.engine.acked_seq;

        if !self.log_hot.batch.is_empty() {
            let bytes = self
                .log_hot
                .commit_async(
                    &mut self.flash,
                    &mut self.sealer,
                    &self.engine.chain,
                    epoch,
                    seq_max,
                )
                .await?;
            self.metrics.add_parity_bytes(bytes.parity_bytes);
            self.metrics.add_marker_bytes(bytes.marker_bytes);
            self.metrics
                .add_padding_bytes(bytes.data_bytes.saturating_sub(bytes.payload_bytes));
            self.metrics.add_commit();
        }
        if !self.log_cold.batch.is_empty() {
            let bytes = self
                .log_cold
                .commit_async(
                    &mut self.flash,
                    &mut self.sealer,
                    &self.engine.chain,
                    epoch,
                    seq_max,
                )
                .await?;
            self.metrics.add_parity_bytes(bytes.parity_bytes);
            self.metrics.add_marker_bytes(bytes.marker_bytes);
            self.metrics
                .add_padding_bytes(bytes.data_bytes.saturating_sub(bytes.payload_bytes));
            self.metrics.add_commit();
        }
        Ok(())
    }

    /// Publishes a checkpoint so garbage collection can advance its reclaim
    /// watermark, WITHOUT advancing the epoch or the hardware counter.
    ///
    /// GC needs a durable record of the index that supersedes a sealed segment
    /// before that segment is eligible for reclaim. It does not need a fresh
    /// epoch. Every reclaim used to go through `seal_epoch_now_async`, so each
    /// one consumed a hardware monotonic-counter increment — on an eFuse-backed
    /// part, a few-thousand-increment lifetime budget being spent on storage
    /// housekeeping rather than on rollback protection.
    pub async fn checkpoint_for_reclaim_async(&mut self) -> Result<(), Error> {
        // Flush the pending batch FIRST.
        //
        // `append_hot` returns the offset the record *will* occupy and the
        // index stores it immediately, but the bytes live in the batch buffer
        // until a commit programs them. Checkpointing the index before that
        // commit persists offsets that address erased flash — and because the
        // checkpoint is what a later mount trusts, every one of those keys
        // comes back as `None` from a database that reported them present
        // before the restart. That is silent data loss across a remount, which
        // is precisely the failure mode SLATE exists to prevent.
        //
        // Program the batch directly rather than calling `commit_inner_async`:
        // this path is reachable from inside that function (commit -> reserve
        // -> refill -> reclaim checkpoint), and re-entering it would make the
        // future's type infinitely sized.
        self.flush_pending_batches_async().await?;

        let index_len = self
            .index
            .serialize(&mut self.ckpt_buf[crate::config::CKPT_HDR_LEN..]);
        let seg_seq = self.log_hot.head.seg_seq;
        let write_offset = self.log_hot.head.write_offset;
        let n_keys = self.index.len() as u16;

        let cost = crate::epoch::checkpoint_only_async(
            &mut self.engine,
            &mut self.flash,
            &mut self.sealer,
            seg_seq,
            write_offset,
            n_keys,
            self.ckpt_buf,
            index_len,
            &mut self.scratch_buf.page_buf,
        )
        .await?;
        self.metrics.add_ckpt_bytes(cost.bytes);
        for _ in 0..cost.erases {
            self.metrics.add_erase();
        }

        self.ckpt_seg_seq = self.segs.current_seg_seq() + 1;
        Ok(())
    }

    /// Writes a checkpoint and opens the next epoch, regardless of how many
    /// records the current epoch holds. `commit` calls this on the Θ trigger;
    /// it is also public so an application can force a checkpoint before a
    /// planned shutdown (bounding the work a later mount has to replay).
    pub async fn seal_epoch_now_async(&mut self) -> Result<(), Error> {
        // Flush the pending batch before capturing the index — see
        // `checkpoint_for_reclaim_async` for why. `append_hot` records the
        // offset a record *will* occupy, so an index serialized while the batch
        // is unflushed persists offsets that address erased flash, and every
        // affected key returns `None` after a remount.
        self.flush_pending_batches_async().await?;

        let index_len = self
            .index
            .serialize(&mut self.ckpt_buf[crate::config::CKPT_HDR_LEN..]);
        let seg_seq = self.log_hot.head.seg_seq;
        let write_offset = self.log_hot.head.write_offset;
        let n_keys = self.index.len() as u16;

        let cost = crate::epoch::seal_epoch_async(
            &mut self.engine,
            &mut self.flash,
            &mut self.counter,
            &mut self.sealer,
            seg_seq,
            write_offset,
            n_keys,
            self.ckpt_buf,
            index_len,
            &mut self.scratch_buf.page_buf,
        )
        .await?;
        self.metrics.add_ckpt_bytes(cost.bytes);
        for _ in 0..cost.erases {
            self.metrics.add_erase();
        }

        // The checkpoint just written durably records the index as of `seg_seq`,
        // so every segment allocated strictly before it is now reclaimable:
        // its live records are reachable from the checkpointed index, and its
        // dead ones can never be needed again. `pick_victim` gates on exactly
        // this watermark, so failing to advance it here leaves GC permanently
        // unable to select a victim.
        // Advance the reclaim watermark past every segment allocated so far.
        // `seg_seq` here is the log head's field, which nothing increments, so
        // using it left the watermark pinned at 0 and `pick_victim`'s
        // `seg_seq < ckpt_seg_seq` test permanently false. The segment table's
        // allocation counter is the real ordering.
        self.ckpt_seg_seq = self.segs.current_seg_seq() + 1;
        Ok(())
    }

    /// Reclaims one segment, sealing an epoch first if nothing is yet eligible.
    ///
    /// A victim must predate the newest checkpoint (`seg_seq < ckpt_seg_seq`), so
    /// on a volume that has sealed segments but has not yet written a checkpoint
    /// this would otherwise return `Ok(())` having done nothing at all — the
    /// caller sees success while no space is freed. Sealing here makes an
    /// explicit `compact()` mean "reclaim if there is anything reclaimable".
    pub async fn compact_async(&mut self) -> Result<(), Error> {
        let in_use = self.live_segments();
        if self
            .segs
            .pick_victim_excluding(self.ckpt_seg_seq, &in_use)
            .is_none()
            && self.segs.count_in_state(SegState::Sealed) > 0
        {
            self.seal_epoch_now_async().await?;
        }
        crate::gc::compact_one_async(self).await
    }

    #[cfg(feature = "blocking")]
    pub fn index_update_offset(&mut self, key: &[u8], new_off: u32) -> Result<(), Error> {
        crate::task::block_on(self.index_update_offset_async(key, new_off))
    }

    #[cfg(feature = "blocking")]
    pub fn index_remove_key(&mut self, key: &[u8]) -> bool {
        crate::task::block_on(self.index_remove_key_async(key))
    }

    /// Blocking form of [`Self::get_into_async`].
    #[cfg(feature = "blocking")]
    pub fn get_into(&mut self, key: &[u8], out: &mut [u8]) -> Option<usize> {
        crate::task::block_on(self.get_into_async(key, out))
    }

    #[cfg(feature = "blocking")]
    pub fn append_cold(&mut self, key: &[u8], val: &[u8], now_ms: u64) -> Result<u32, Error> {
        crate::task::block_on(self.append_cold_async(key, val, now_ms))
    }

    #[cfg(feature = "blocking")]
    pub fn append_cold_tombstone(&mut self, key: &[u8], now_ms: u64) -> Result<(), Error> {
        crate::task::block_on(self.append_cold_tombstone_async(key, now_ms))
    }

    #[cfg(feature = "blocking")]
    pub fn commit(&mut self) -> Result<(), Error> {
        crate::task::block_on(self.commit_async())
    }

    #[cfg(feature = "blocking")]
    pub fn seal_epoch_now(&mut self) -> Result<(), Error> {
        crate::task::block_on(self.seal_epoch_now_async())
    }

    #[cfg(feature = "blocking")]
    pub fn compact(&mut self) -> Result<(), Error> {
        crate::task::block_on(self.compact_async())
    }
    /// Milliseconds until the scheduler wants the next commit, or `None` when
    /// the batch is empty and nothing is pending. Lets an Embassy task sleep
    /// with `Timer::after` for exactly this long instead of polling — the
    /// scheduler keeps ownership of the energy policy (doc 008), the executor
    /// merely honours it.
    pub fn next_commit_deadline_ms(&self, now_ms: u64) -> Option<u64> {
        if self.sched.ops_since_commit == 0 {
            None
        } else {
            let deadline = self.sched.oldest_pending_ms + self.sched.cfg.deadline_ms as u64;
            Some(deadline.saturating_sub(now_ms))
        }
    }
}

/// Copies the record at `off` (header + ciphertext + tag) into `hdr_out`/`rec_out`,
/// looking first in the hot batch, then the cold batch, then flash. Returns the
/// record's total length on success. The batch checks are essential: an offset
/// that was just handed out by `Log::append` points into a not-yet-flushed batch,
/// so a flash read there would see the erased/old page.
#[allow(clippy::too_many_arguments)]
async fn read_candidate_async<F: slate_kv_hal::AsyncFlash>(
    flash: &mut F,
    hot_base: u32,
    hot_data: &[u8],
    cold_base: u32,
    cold_data: &[u8],
    off: u32,
    hdr_out: &mut [u8; REC_HDR_LEN],
    rec_out: &mut [u8],
) -> Option<usize> {
    for (base, data) in [(hot_base, hot_data), (cold_base, cold_data)] {
        if off >= base {
            let ro = (off - base) as usize;
            if ro + REC_HDR_LEN <= data.len() {
                hdr_out.copy_from_slice(&data[ro..ro + REC_HDR_LEN]);
                let hdr = crate::record::RecordHeader::decode(hdr_out).ok()?;
                let total = REC_OVERHEAD + hdr.klen as usize + hdr.vlen as usize;
                if ro + total <= data.len() && total <= rec_out.len() {
                    rec_out[..total].copy_from_slice(&data[ro..ro + total]);
                    return Some(total);
                }
            }
        }
    }

    if flash.read(off, hdr_out).await.is_err() {
        return None;
    }
    let hdr = crate::record::RecordHeader::decode(hdr_out).ok()?;
    let total = REC_OVERHEAD + hdr.klen as usize + hdr.vlen as usize;
    if total > rec_out.len() {
        return None;
    }
    if flash.read(off, &mut rec_out[..total]).await.is_err() {
        return None;
    }
    Some(total)
}

/// Returns whether the record at `cand_off` decrypts to exactly `key`. Used to
/// resolve fingerprint collisions when updating/removing index entries.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cand_matches_key_async<F: slate_kv_hal::AsyncFlash, S: Sealer>(
    flash: &mut F,
    sealer: &mut S,
    hot_base: u32,
    hot_data: &[u8],
    cold_base: u32,
    cold_data: &[u8],
    cand_rec_bytes: &mut [u8],
    cand_scratch: &mut [u8],
    cand_off: u32,
    key: &[u8],
) -> bool {
    let mut hdr_bytes = [0u8; REC_HDR_LEN];
    let total = match read_candidate_async(
        flash,
        hot_base,
        hot_data,
        cold_base,
        cold_data,
        cand_off,
        &mut hdr_bytes,
        cand_rec_bytes,
    )
    .await
    {
        Some(t) => t,
        None => return false,
    };
    let hdr = match crate::record::RecordHeader::decode(&hdr_bytes) {
        Ok(h) => h,
        Err(_) => return false,
    };
    if hdr.klen as usize != key.len() {
        return false;
    }
    if sealer
        .open_record(
            &hdr_bytes,
            &cand_rec_bytes[REC_HDR_LEN..total],
            cand_scratch,
        )
        .is_err()
    {
        return false;
    }
    &cand_scratch[..hdr.klen as usize] == key
}
