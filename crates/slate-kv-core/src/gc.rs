//! gc
#![allow(missing_docs)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::needless_range_loop)]

use crate::error::Error;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SegState {
    OpenHot,
    OpenCold,
    Sealed,
    Free,
}

#[derive(Clone, Copy, Debug)]
pub struct SegEntry {
    pub id: u32,
    pub live_bytes: u16,
    pub minseq: u64,
    pub state: SegState,
    pub seg_seq: u64,
}

impl SegEntry {
    pub fn new(id: u32) -> Self {
        Self {
            id,
            live_bytes: 0,
            minseq: u64::MAX,
            state: SegState::Free,
            seg_seq: 0,
        }
    }

    pub fn reset_to_free(&mut self) {
        self.live_bytes = 0;
        self.minseq = u64::MAX;
        self.state = SegState::Free;
        self.seg_seq = 0;
    }
}

pub const MAX_SEGMENTS: usize = 128;

/// Number of whole segments that fit in `[base, capacity)`, capped at
/// [`MAX_SEGMENTS`].
///
/// Use this rather than passing a hardcoded segment count: a table claiming more
/// segments than the region holds lets GC pick a victim whose base address lies
/// past the end of flash, and a table claiming fewer leaves an unreclaimable
/// tail that the log head will eventually run into.
pub fn segments_in(base: u32, capacity: u32) -> u32 {
    if capacity <= base {
        return 0;
    }
    let n = (capacity - base) / crate::config::SEG_BYTES as u32;
    core::cmp::min(n, MAX_SEGMENTS as u32)
}

pub struct SegTable {
    pub entries: [SegEntry; MAX_SEGMENTS],
    pub num_segments: u32,
    /// Records walked by the most recent compaction.
    ///
    /// Kept here rather than in `Metrics` because `Metrics` is behind an
    /// off-by-default feature, and this feeds a correctness guard: when
    /// compaction relocates nearly everything it reads, the live set no longer
    /// fits and the engine must report `FlashFull` instead of copying forever.
    pub last_gc_scanned: u32,
    /// Records the most recent compaction found still live and had to relocate.
    pub last_gc_relocated: u32,
    /// Next segment allocation number to hand out.
    ///
    /// `seg_seq` is an *allocation* number, not a segment index: victim
    /// selection compares it against the newest checkpoint's `seg_seq` to decide
    /// whether a sealed segment predates that checkpoint. Nothing used to
    /// advance it (the log head's `seg_seq` stayed 0 forever), so the
    /// `seg_seq < ckpt_seg_seq` test in [`Self::pick_victim`] was never true and
    /// GC could not pick a victim even once segments were being sealed.
    pub next_seg_seq: u64,
    /// Flash offset of segment 0.
    ///
    /// Segments tile the log area *above* the reserved superblock and
    /// checkpoint slots, so this is normally `config::data_base_offset()`. It
    /// is not implicitly zero: with `base_addr == 0`, `seg_base(0)` lands on
    /// the superblock and GC's reclaim erase would wipe the live checkpoint
    /// slots instead of a data segment.
    pub base_addr: u32,
}

impl SegTable {
    /// Creates a table of `num_segments` segments starting at offset 0.
    ///
    /// Prefer [`Self::with_base`]: a log that starts above a reserved region
    /// (every real volume does) needs a matching segment base, or segment
    /// addresses and log addresses describe different parts of the chip.
    pub fn new(num_segments: u32) -> Self {
        Self::with_base(0, num_segments)
    }

    /// Creates a table whose segment 0 begins at `base_addr`.
    pub fn with_base(base_addr: u32, num_segments: u32) -> Self {
        let mut entries = [SegEntry::new(0); MAX_SEGMENTS];
        let n = core::cmp::min(num_segments as usize, MAX_SEGMENTS);
        for (i, e) in entries.iter_mut().enumerate().take(n) {
            *e = SegEntry::new(i as u32);
        }
        Self {
            entries,
            num_segments: n as u32,
            next_seg_seq: 1,
            base_addr,
            last_gc_scanned: 0,
            last_gc_relocated: 0,
        }
    }

    /// Flash offset of segment `id`.
    #[inline]
    pub fn seg_base(&self, id: u32) -> u32 {
        self.base_addr + id * crate::config::SEG_BYTES as u32
    }

    /// First offset past the last segment. The log head must never reach this:
    /// bytes above it belong to no segment, so GC can never reclaim them.
    #[inline]
    pub fn end_addr(&self) -> u32 {
        self.seg_base(self.num_segments)
    }

    /// The segment containing `off`, or `None` if `off` lies outside the
    /// segment-managed area (below `base_addr` or in the unmanaged tail).
    #[inline]
    pub fn seg_of(&self, off: u32) -> Option<u32> {
        if off < self.base_addr || off >= self.end_addr() {
            return None;
        }
        Some((off - self.base_addr) / crate::config::SEG_BYTES as u32)
    }

    pub fn pick_victim(&self, ckpt_seg_seq: u64) -> Option<u32> {
        self.pick_victim_excluding(ckpt_seg_seq, &[])
    }

    /// Like [`Self::pick_victim`] but never returns a segment in `in_use`.
    ///
    /// Reclaim erases all twelve blocks of the victim, so a segment that a log
    /// head currently occupies must never be selected: the cold head in
    /// particular starts at `data_base` and stays there until the cold log is
    /// written, which is exactly where the oldest records live. Selecting it
    /// erases live data that the scan never even visits — the scan starts at the
    /// segment base and stops at the first erased byte, so a segment whose first
    /// page belongs to the *hot* log looks empty (`gc_scanned == 0`) while
    /// holding thousands of live records.
    pub fn pick_victim_excluding(&self, ckpt_seg_seq: u64, in_use: &[u32]) -> Option<u32> {
        self.entries[..self.num_segments as usize]
            .iter()
            .filter(|s| {
                s.state == SegState::Sealed && s.seg_seq < ckpt_seg_seq && !in_use.contains(&s.id)
            })
            .min_by_key(|s| s.live_bytes)
            .map(|s| s.id)
    }

    /// Lowest-id segment currently free, if any.
    pub fn pick_free(&self) -> Option<u32> {
        self.entries[..self.num_segments as usize]
            .iter()
            .find(|s| s.state == SegState::Free)
            .map(|s| s.id)
    }

    /// A free segment that no log head currently occupies.
    ///
    /// This is the allocator's supply for the circular log. [`Self::pick_free`]
    /// alone is not sufficient: a head sitting in a segment that the table still
    /// calls `Free` (which happens for the cold head before its first write)
    /// would be handed out to the other log, and the two would then interleave
    /// records in one segment that reclaim erases as a unit.
    ///
    /// Selection is by lowest id rather than lowest erase count. Wear levelling
    /// comes from the reclaim cycle visiting every segment, not from allocation
    /// order — the log sweeps the whole region before returning to any segment,
    /// so erase counts stay within one of each other regardless of which free
    /// segment is chosen first.
    pub fn pick_free_excluding(&self, in_use: &[u32]) -> Option<u32> {
        self.entries[..self.num_segments as usize]
            .iter()
            .find(|s| s.state == SegState::Free && !in_use.contains(&s.id))
            .map(|s| s.id)
    }

    pub fn free_count(&self) -> u32 {
        self.entries[..self.num_segments as usize]
            .iter()
            .filter(|s| s.state == SegState::Free)
            .count() as u32
    }

    pub fn count_in_state(&self, want: SegState) -> u32 {
        self.entries[..self.num_segments as usize]
            .iter()
            .filter(|s| s.state == want)
            .count() as u32
    }

    /// Marks the segment containing `off` as open in `state`, sealing whichever
    /// segment was previously open in that state.
    ///
    /// This is the transition no production path used to perform. Because
    /// [`Self::pick_victim`] only considers `Sealed` segments, a table whose
    /// entries never leave `SegState::Free` makes compaction a permanent no-op:
    /// the log head runs to the end of the region and every subsequent program
    /// fails. `seg_seq`/`minseq` are stamped on open so victim selection can
    /// tell a segment that predates the newest checkpoint from one that does
    /// not.
    ///
    /// Returns the segment id now open, or `None` if `off` is outside the
    /// managed area.
    pub fn open_at(&mut self, off: u32, seg_seq: u64, minseq: u64, state: SegState) -> Option<u32> {
        let cur = self.seg_of(off)?;
        for i in 0..self.num_segments {
            if i == cur {
                continue;
            }
            let e = &mut self.entries[i as usize];
            if e.state == state {
                e.state = SegState::Sealed;
            }
        }
        let next = self.next_seg_seq;
        let e = &mut self.entries[cur as usize];
        if e.state == SegState::Free {
            e.seg_seq = next;
            e.minseq = minseq;
            self.next_seg_seq = next + 1;
        }
        let _ = seg_seq;
        e.state = state;
        Some(cur)
    }

    /// Allocation number of the newest segment handed out. A checkpoint taken
    /// now supersedes every segment sealed at or below this number.
    pub fn current_seg_seq(&self) -> u64 {
        self.next_seg_seq.saturating_sub(1)
    }

    /// Reconstructs segment state by reading the on-flash headers.
    ///
    /// **Mount must call this before the allocator runs.** A freshly
    /// constructed table marks every segment `Free`, so on a volume that
    /// already holds data the first head roll would hand out a segment full of
    /// live records and erase it. The headers are the durable record of which
    /// segments are in use and in what order they were allocated.
    ///
    /// The segment holding the largest `seg_seq` is the one the log was writing
    /// when it stopped, so it is reopened as `OpenHot`; every other segment
    /// carrying a header is `Sealed`. `next_seg_seq` resumes above the highest
    /// number seen, so allocation numbers stay monotone across a remount and
    /// the reclaim watermark keeps its meaning.
    ///
    /// Returns the number of segments found to be in use.
    pub async fn rebuild_from_flash<F: slate_kv_hal::AsyncFlash>(&mut self, flash: &mut F) -> u32 {
        let mut max_seq = 0u64;
        let mut newest: Option<u32> = None;
        let mut in_use = 0u32;

        for i in 0..self.num_segments {
            let base = self.seg_base(i);
            let e = &mut self.entries[i as usize];
            match crate::segment::read_header(flash, base).await {
                Some(hdr) => {
                    e.state = SegState::Sealed;
                    e.seg_seq = hdr.seg_seq;
                    e.minseq = hdr.minseq;
                    in_use += 1;
                    if hdr.seg_seq >= max_seq {
                        max_seq = hdr.seg_seq;
                        newest = Some(i);
                    }
                }
                None => e.reset_to_free(),
            }
            crate::task::yield_now().await;
        }

        if let Some(id) = newest {
            self.entries[id as usize].state = SegState::OpenHot;
        }
        self.next_seg_seq = max_seq + 1;
        in_use
    }

    /// The live segment with the lowest allocation number.
    ///
    /// This is where the log begins. Recovery falls back to it when the
    /// checkpoint's recorded head has since been reclaimed — replaying from a
    /// stale offset inside a freed segment reconstructs nothing and then walks
    /// backwards through older segments, which loses the whole index.
    pub fn oldest_live_segment(&self) -> Option<u32> {
        self.entries[..self.num_segments as usize]
            .iter()
            .filter(|s| s.state != SegState::Free)
            .min_by_key(|s| s.seg_seq)
            .map(|s| s.id)
    }

    /// The segment that follows `after_seq` in allocation order, if any.
    ///
    /// Recovery walks the log in allocation order rather than address order.
    /// Once the head can wrap, the two differ: the segment holding the newest
    /// records may sit at a lower address than the one holding the oldest, so
    /// an address-ordered replay reads the log backwards and reconstructs an
    /// index from superseded records.
    pub fn next_in_seq_order(&self, after_seq: u64) -> Option<u32> {
        self.entries[..self.num_segments as usize]
            .iter()
            .filter(|s| s.state != SegState::Free && s.seg_seq > after_seq)
            .min_by_key(|s| s.seg_seq)
            .map(|s| s.id)
    }

    /// First offset past the writable area of segment `id`.
    ///
    /// The log may only use the 8 data blocks; the 4 parity blocks above them
    /// belong to `segment::encode_parity` at seal time. Writing past this into
    /// the parity blocks is what made RS(12,8) unencodable in practice.
    #[inline]
    pub fn seg_data_end(&self, id: u32) -> u32 {
        self.seg_base(id) + crate::config::SEG_DATA_BYTES as u32
    }

    /// True if `need` bytes written at `off` stay inside the *data* area of
    /// `off`'s own segment.
    ///
    /// A commit must not straddle a segment boundary: GC reclaims whole
    /// segments, so a record spanning two could be left half-erased — and the
    /// compaction scan, which starts at a segment base and stops at the first
    /// erased byte, cannot walk a segment whose first byte is mid-ciphertext.
    /// That was the second of the two format-level causes of the wrap failure.
    pub fn fits_in_segment(&self, off: u32, need: u32) -> bool {
        match self.seg_of(off) {
            Some(id) => off.saturating_add(need) <= self.seg_data_end(id),
            None => false,
        }
    }

    pub fn update_live_bytes(&mut self, id: u32, delta: i32) {
        if id < self.num_segments {
            let entry = &mut self.entries[id as usize];
            let current = entry.live_bytes as i32;
            let new_val = core::cmp::max(0, current + delta);
            entry.live_bytes = new_val as u16;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn reappend_cold<F: slate_kv_hal::AsyncFlash, S: crate::log::Sealer>(
    log_cold: &mut crate::log::Log<'_, F>,
    sealer: &mut S,
    engine: &mut crate::epoch::EngineState,
    sched: &mut crate::sched::Scheduler,
    op: u8,
    key: &[u8],
    val: &[u8],
) -> Result<(u32, bool), crate::error::Error> {
    let seq = engine.next_seq;
    let epoch = engine.epoch;
    let (_seq_ret, offset) =
        log_cold.append(seq, epoch, op, key, val, sealer, &mut engine.chain)?;
    engine.next_seq += 1;
    engine.records_in_epoch += 1;
    let need_commit = sched.on_append(0);
    Ok((offset, need_commit))
}

#[allow(clippy::too_many_arguments)]
async fn reindex_update_offset<F: slate_kv_hal::AsyncFlash, S: crate::log::Sealer>(
    index: &mut crate::index::Index<'_>,
    rng: &mut crate::index::XorShift64,
    flash: &mut F,
    sealer: &mut S,
    log_hot: &crate::log::Log<'_, F>,
    log_cold: &crate::log::Log<'_, F>,
    cand_rec: &mut [u8],
    cand_scratch: &mut [u8],
    key: &[u8],
    new_off: u32,
) -> Result<(), crate::error::Error> {
    let hot_base = log_hot.head.write_offset;
    let hot_data = log_hot.batch.data();
    let cold_base = log_cold.head.write_offset;
    let cold_data = log_cold.batch.data();
    let mut cbuf = crate::index::CandidateBuf::new();
    index.candidates(key, &mut cbuf);
    let mut matching_off = None;
    for &cand_off in cbuf.as_slice() {
        if crate::slate::cand_matches_key_async(
            flash,
            sealer,
            hot_base,
            hot_data,
            cold_base,
            cold_data,
            cand_rec,
            cand_scratch,
            cand_off,
            key,
        )
        .await
        {
            matching_off = Some(cand_off);
            break;
        }
    }
    index.upsert(key, new_off, rng, |cand_off| Some(cand_off) == matching_off)
}

pub async fn compact_one_async<
    'a,
    F: slate_kv_hal::AsyncFlash,
    C: slate_kv_hal::AsyncMonotonicCounter,
    S: crate::log::Sealer,
>(
    st: &mut crate::slate::Slate<'a, F, C, S>,
) -> Result<(), Error> {
    let ckpt_seg_seq = st.ckpt_seg_seq;
    // Never reclaim a segment a log head currently occupies.
    let in_use = st.live_segments();
    let victim = match st.segs.pick_victim_excluding(ckpt_seg_seq, &in_use) {
        Some(v) => v,
        None => return Ok(()),
    };

    assert!(
        st.segs.entries[victim as usize].seg_seq < ckpt_seg_seq,
        "Victim is newer than latest checkpoint"
    );

    // We stub the scan of the segment for now
    let watermark = st.segs.entries[..st.segs.num_segments as usize]
        .iter()
        .filter(|s| s.id != victim && s.state != SegState::Free)
        .map(|s| s.minseq)
        .min()
        .unwrap_or(u64::MAX);

    // Real record scan. `seg_base` must go through the table: segments tile the
    // log area above the reserved region, so `victim * SEG_BYTES` alone points
    // at the superblock/checkpoint slots for low ids.
    let seg_base = st.segs.seg_base(victim);
    let page_size = st.flash.page_size() as u32;

    // Segments now carry an on-flash header in their first page, so records
    // start one page in. Scanning from `seg_base` would decode the header's
    // magic as a record and abort the walk at the first byte, leaving every
    // live record in the segment unrelocated — and then erased.
    let has_header = crate::segment::read_header(&mut st.flash, seg_base)
        .await
        .is_some();
    let mut off = if has_header {
        seg_base + page_size
    } else {
        seg_base
    };

    let mut buf = [0u8; 1];
    let mut since_yield = 0u16;
    let mut scanned = 0u32;
    let mut relocated = 0u32;

    // Stop at the end of the DATA area, not the end of the 12-block stride: the
    // parity blocks above it hold RS symbols, not records, and decoding them as
    // records would at best abort the scan and at worst relocate garbage.
    let scan_end = seg_base + crate::config::SEG_DATA_BYTES as u32;

    while off < scan_end {
        if since_yield >= crate::config::GC_YIELD_EVERY_RECORDS {
            crate::task::yield_now().await;
            since_yield = 0;
        }
        since_yield += 1;
        if st.flash.read(off, &mut buf).await.is_err() {
            break;
        }

        match buf[0] {
            crate::config::ERASED_BYTE => {
                let rem = off % page_size;
                if rem != 0 {
                    off += page_size - rem;
                } else {
                    break;
                }
            }
            crate::config::MAGIC_CM => {
                off += page_size * 2;
            }
            crate::config::MAGIC_XOR => {
                off += page_size;
            }
            crate::config::MAGIC_REC => {
                let mut hdr_bytes = [0u8; crate::config::REC_HDR_LEN];
                if st.flash.read(off, &mut hdr_bytes).await.is_err() {
                    break;
                }
                if let Ok(hdr) = crate::record::RecordHeader::decode(&hdr_bytes) {
                    scanned += 1;
                    st.metrics.add_gc_scanned();
                    let total_len =
                        crate::config::REC_OVERHEAD + hdr.klen as usize + hdr.vlen as usize;
                    if total_len <= st.scratch_buf.gc_rec_bytes.len()
                        && st
                            .flash
                            .read(off, &mut st.scratch_buf.gc_rec_bytes[..total_len])
                            .await
                            .is_ok()
                    {
                        let opened = st
                            .sealer
                            .open_record(
                                &hdr_bytes,
                                &st.scratch_buf.gc_rec_bytes[crate::config::REC_HDR_LEN..total_len],
                                &mut st.scratch_buf.gc_scratch,
                            )
                            .is_ok();
                        if !opened {
                            st.metrics.add_gc_open_failed();
                        }
                        if opened {
                            let key_len = hdr.klen as usize;
                            let val_len = hdr.vlen as usize;
                            let _ = val_len;
                            let key = &st.scratch_buf.gc_scratch[..key_len];
                            let val = &st.scratch_buf.gc_scratch[key_len..key_len + val_len];

                            if hdr.op == crate::config::OP_PUT {
                                let mut is_live = false;
                                let mut cbuf = crate::index::CandidateBuf::new();
                                st.index.candidates(key, &mut cbuf);
                                if cbuf.as_slice().contains(&off) {
                                    is_live = true;
                                }
                                if is_live {
                                    let (new_off, need_commit) = reappend_cold(
                                        &mut st.log_cold,
                                        &mut st.sealer,
                                        &mut st.engine,
                                        &mut st.sched,
                                        crate::config::OP_PUT,
                                        key,
                                        val,
                                    )?;
                                    // Relocation traffic: bytes written by GC
                                    // rather than by the application. This is
                                    // the numerator term that makes write
                                    // amplification differ from 1.0.
                                    st.metrics.add_gc_bytes(total_len as u64);
                                    st.metrics.add_gc_relocated();
                                    relocated += 1;
                                    if need_commit {
                                        st.commit_inner_async().await?;
                                    }
                                    let key = &st.scratch_buf.gc_scratch[..key_len];
                                    reindex_update_offset(
                                        &mut st.index,
                                        &mut st.rng,
                                        &mut st.flash,
                                        &mut st.sealer,
                                        &st.log_hot,
                                        &st.log_cold,
                                        &mut st.scratch_buf.cand_rec_bytes,
                                        &mut st.scratch_buf.cand_scratch,
                                        key,
                                        new_off,
                                    )
                                    .await?;
                                }
                            } else if hdr.op == crate::config::OP_DEL && hdr.seq > watermark {
                                let (_, need_commit) = reappend_cold(
                                    &mut st.log_cold,
                                    &mut st.sealer,
                                    &mut st.engine,
                                    &mut st.sched,
                                    crate::config::OP_DEL,
                                    key,
                                    &[],
                                )?;
                                st.metrics
                                    .add_gc_bytes((crate::config::REC_OVERHEAD + key_len) as u64);
                                if need_commit {
                                    st.commit_inner_async().await?;
                                }
                            }
                        }
                    }
                    off += total_len as u32;
                } else {
                    break;
                }
            }
            _ => {
                break;
            }
        }
    }

    st.commit_inner_async().await?;

    // Guard against erasing a segment whose contents were never walked.
    //
    // The scan stops at the first erased byte, so before segments carried
    // headers a genuinely empty segment and one whose first page belonged to
    // the *other* log both yielded `scanned == 0` — and reclaiming on that
    // basis destroyed live records. A valid header removes the ambiguity: it
    // proves this segment is one this engine opened, that records begin exactly
    // one page in, and therefore that the scan walked the whole of it. Absent a
    // header, fall back to the old conservative test.
    if !has_header {
        let mut first = [0u8; 1];
        let first_is_erased = st.flash.read(seg_base, &mut first).await.is_ok()
            && first[0] == crate::config::ERASED_BYTE;
        if scanned == 0 && !first_is_erased {
            // Not provably reclaimable: leave it sealed rather than risk data loss.
            return Ok(());
        }
    }

    for b in 0..(crate::config::SEG_BLOCKS_DATA + crate::config::SEG_BLOCKS_PARITY) {
        st.flash
            .erase(seg_base + (b as u32) * st.flash.block_size() as u32)
            .await
            .map_err(|_| Error::Io)?;
        st.metrics.add_erase();
        crate::task::yield_now().await;
    }
    // Publish this compaction's live ratio so `refill_free_segments_async` can
    // tell productive reclaim from futile copying. Recorded after the erase, so
    // it always reflects a completed pass.
    st.segs.last_gc_scanned = scanned;
    st.segs.last_gc_relocated = relocated;

    st.segs.entries[victim as usize].reset_to_free();
    st.metrics.add_gc_segment_freed();
    Ok(())
}
