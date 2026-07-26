//! slate
#![allow(missing_docs)]

use crate::config::*;
use crate::epoch::EngineState;
use crate::error::Error;
use crate::gc::SegTable;
use crate::index::Index;
use crate::log::{Log, Sealer};
use slate_kv_hal::{Flash, MonotonicCounter};

pub struct Slate<'a, F: Flash, C: MonotonicCounter, S: Sealer> {
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
}

impl<'a, F: Flash, C: MonotonicCounter, S: Sealer> Slate<'a, F, C, S> {
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
    pub fn index_update_offset(&mut self, key: &[u8], new_off: u32) -> Result<(), Error> {
        let flash = &mut self.flash;
        let sealer = &mut self.sealer;
        let hot_base = self.log_hot.head.write_offset;
        let hot_data = self.log_hot.batch.data();
        let cold_base = self.log_cold.head.write_offset;
        let cold_data = self.log_cold.batch.data();
        self.index.upsert(key, new_off, &mut self.rng, |cand_off| {
            cand_matches_key(
                flash, sealer, hot_base, hot_data, cold_base, cold_data, cand_off, key,
            )
        })
    }

    /// Removes `key` from the index, matching the *full* key so a fingerprint
    /// collision cannot evict a different live key's slot. Returns whether an
    /// entry was removed.
    pub fn index_remove_key(&mut self, key: &[u8]) -> bool {
        let flash = &mut self.flash;
        let sealer = &mut self.sealer;
        let hot_base = self.log_hot.head.write_offset;
        let hot_data = self.log_hot.batch.data();
        let cold_base = self.log_cold.head.write_offset;
        let cold_data = self.log_cold.batch.data();
        self.index.remove(key, |cand_off| {
            cand_matches_key(
                flash, sealer, hot_base, hot_data, cold_base, cold_data, cand_off, key,
            )
        })
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

    pub fn append_cold(&mut self, key: &[u8], val: &[u8], now_ms: u64) -> Result<u32, Error> {
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
            self.commit()?;
        }
        Ok(offset)
    }

    pub fn append_cold_tombstone(&mut self, key: &[u8], now_ms: u64) -> Result<(), Error> {
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
            self.commit()?;
        }
        Ok(())
    }

    pub fn cold_batch_full(&self) -> bool {
        // Assume batch full if offset > limit
        self.log_cold.batch.data().len() >= 1024
    }

    pub fn commit(&mut self) -> Result<(), Error> {
        let seq_max = self.engine.next_seq.saturating_sub(1);
        self.log_hot.commit(
            &mut self.flash,
            &mut self.sealer,
            &self.engine.chain,
            self.engine.epoch,
            seq_max,
        )?;
        self.log_cold.commit(
            &mut self.flash,
            &mut self.sealer,
            &self.engine.chain,
            self.engine.epoch,
            seq_max,
        )?;
        self.engine.acked_seq = seq_max;
        self.sched.on_commit();
        self.metrics.add_commit();
        // Each durable commit wakes the flash subsystem from low-power sleep;
        // this is the E_wake half of the fixed commit cost A (§8.1) and feeds the
        // energy model in slate-sim.
        self.metrics.add_wake();

        if self.engine.records_in_epoch >= crate::config::THETA {
            self.seal_epoch_now()?;
        }
        Ok(())
    }

    /// Writes a checkpoint and opens the next epoch, regardless of how many
    /// records the current epoch holds. `commit` calls this on the Θ trigger;
    /// it is also public so an application can force a checkpoint before a
    /// planned shutdown (bounding the work a later mount has to replay).
    pub fn seal_epoch_now(&mut self) -> Result<(), Error> {
        let index_len = self
            .index
            .serialize(&mut self.ckpt_buf[crate::config::CKPT_HDR_LEN..]);
        let seg_seq = self.log_hot.head.seg_seq;
        let write_offset = self.log_hot.head.write_offset;
        let n_keys = self.index.len() as u16;

        crate::epoch::seal_epoch(
            &mut self.engine,
            &mut self.flash,
            &mut self.counter,
            &mut self.sealer,
            seg_seq,
            write_offset,
            n_keys,
            self.ckpt_buf,
            index_len,
        )?;

        // The checkpoint just written durably records the index as of `seg_seq`,
        // so every segment allocated strictly before it is now reclaimable:
        // its live records are reachable from the checkpointed index, and its
        // dead ones can never be needed again. `pick_victim` gates on exactly
        // this watermark, so failing to advance it here leaves GC permanently
        // unable to select a victim.
        self.ckpt_seg_seq = seg_seq;
        Ok(())
    }

    pub fn compact(&mut self) -> Result<(), Error> {
        crate::gc::compact_one(self)
    }
}

/// Copies the record at `off` (header + ciphertext + tag) into `hdr_out`/`rec_out`,
/// looking first in the hot batch, then the cold batch, then flash. Returns the
/// record's total length on success. The batch checks are essential: an offset
/// that was just handed out by `Log::append` points into a not-yet-flushed batch,
/// so a flash read there would see the erased/old page.
#[allow(clippy::too_many_arguments)]
fn read_candidate<F: Flash>(
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

    if flash.read(off, hdr_out).is_err() {
        return None;
    }
    let hdr = crate::record::RecordHeader::decode(hdr_out).ok()?;
    let total = REC_OVERHEAD + hdr.klen as usize + hdr.vlen as usize;
    if total > rec_out.len() {
        return None;
    }
    if flash.read(off, &mut rec_out[..total]).is_err() {
        return None;
    }
    Some(total)
}

/// Returns whether the record at `cand_off` decrypts to exactly `key`. Used to
/// resolve fingerprint collisions when updating/removing index entries.
#[allow(clippy::too_many_arguments)]
fn cand_matches_key<F: Flash, S: Sealer>(
    flash: &mut F,
    sealer: &mut S,
    hot_base: u32,
    hot_data: &[u8],
    cold_base: u32,
    cold_data: &[u8],
    cand_off: u32,
    key: &[u8],
) -> bool {
    let mut hdr_bytes = [0u8; REC_HDR_LEN];
    let mut rec_bytes = [0u8; REC_OVERHEAD + MAX_KEY_LEN + MAX_VAL_LEN];
    let total = match read_candidate(
        flash,
        hot_base,
        hot_data,
        cold_base,
        cold_data,
        cand_off,
        &mut hdr_bytes,
        &mut rec_bytes,
    ) {
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
    let mut scratch = [0u8; MAX_KEY_LEN + MAX_VAL_LEN];
    if sealer
        .open_record(&hdr_bytes, &rec_bytes[REC_HDR_LEN..total], &mut scratch)
        .is_err()
    {
        return false;
    }
    &scratch[..hdr.klen as usize] == key
}
