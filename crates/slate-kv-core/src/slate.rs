//! slate
#![allow(missing_docs)]

use crate::config::*;
use crate::epoch::EngineState;
use crate::error::Error;
use crate::gc::SegTable;
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

    pub async fn commit_async(&mut self) -> Result<(), Error> {
        let seq_max = self.engine.next_seq.saturating_sub(1);
        self.log_hot
            .commit_async(
                &mut self.flash,
                &mut self.sealer,
                &self.engine.chain,
                self.engine.epoch,
                seq_max,
            )
            .await?;
        self.log_cold
            .commit_async(
                &mut self.flash,
                &mut self.sealer,
                &self.engine.chain,
                self.engine.epoch,
                seq_max,
            )
            .await?;
        self.engine.acked_seq = seq_max;
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

    /// Writes a checkpoint and opens the next epoch, regardless of how many
    /// records the current epoch holds. `commit` calls this on the Θ trigger;
    /// it is also public so an application can force a checkpoint before a
    /// planned shutdown (bounding the work a later mount has to replay).
    pub async fn seal_epoch_now_async(&mut self) -> Result<(), Error> {
        let index_len = self
            .index
            .serialize(&mut self.ckpt_buf[crate::config::CKPT_HDR_LEN..]);
        let seg_seq = self.log_hot.head.seg_seq;
        let write_offset = self.log_hot.head.write_offset;
        let n_keys = self.index.len() as u16;

        crate::epoch::seal_epoch_async(
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

        // The checkpoint just written durably records the index as of `seg_seq`,
        // so every segment allocated strictly before it is now reclaimable:
        // its live records are reachable from the checkpointed index, and its
        // dead ones can never be needed again. `pick_victim` gates on exactly
        // this watermark, so failing to advance it here leaves GC permanently
        // unable to select a victim.
        self.ckpt_seg_seq = seg_seq;
        Ok(())
    }

    pub async fn compact_async(&mut self) -> Result<(), Error> {
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
