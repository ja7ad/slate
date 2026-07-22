//! slate
#![allow(missing_docs)]

use crate::config::*;
use crate::epoch::EngineState;
use crate::error::Error;
use crate::gc::SegTable;
use crate::index::Index;
use crate::log::{Log, Sealer};
use slate_hal::{Flash, MonotonicCounter};

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

    pub fn index_update_offset(&mut self, key: &[u8], new_off: u32) {
        let flash = &mut self.flash;
        let sealer = &mut self.sealer;
        let _ = self.index.upsert(key, new_off, &mut self.rng, |cand_off| {
            let mut hdr_bytes = [0u8; REC_HDR_LEN];
            if flash.read(cand_off, &mut hdr_bytes).is_err() { return false; }
            let hdr = match crate::record::RecordHeader::decode(&hdr_bytes) {
                Ok(h) => h,
                Err(_) => return false,
            };
            if hdr.klen as usize != key.len() { return false; }
            let total_len = 44 + hdr.klen as usize + hdr.vlen as usize;
            // Bound check just in case, though klen/vlen max checks are in decode
            if total_len > 44 + MAX_KEY_LEN + MAX_VAL_LEN { return false; }
            
            let mut rec_bytes = [0u8; 44 + MAX_KEY_LEN + MAX_VAL_LEN];
            if flash.read(cand_off, &mut rec_bytes[..total_len]).is_err() { return false; }
            
            let mut scratch = [0u8; MAX_KEY_LEN + MAX_VAL_LEN];
            if sealer.open_record(&hdr_bytes, &rec_bytes[REC_HDR_LEN..total_len], &mut scratch).is_err() {
                return false;
            }
            &scratch[..hdr.klen as usize] == key
        });
    }

    pub fn append_cold(&mut self, key: &[u8], val: &[u8], now_ms: u64) -> Result<u32, Error> {
        let seq = self.engine.next_seq;
        let (_seq_ret, offset) = self.log_cold.append(
            seq,
            OP_PUT,
            key,
            val,
            &mut self.sealer,
            &mut self.engine.chain,
        )?;
        self.engine.next_seq += 1;
        if self.sched.on_append(now_ms) {
            self.commit()?;
        }
        Ok(offset)
    }

    pub fn append_cold_tombstone(&mut self, key: &[u8], now_ms: u64) -> Result<(), Error> {
        let seq = self.engine.next_seq;
        let _ = self.log_cold.append(
            seq,
            OP_DEL,
            key,
            &[],
            &mut self.sealer,
            &mut self.engine.chain,
        )?;
        self.engine.next_seq += 1;
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

        if self.engine.records_in_epoch >= crate::config::THETA {
            let index_len = self
                .index
                .serialize(&mut self.ckpt_buf[crate::checkpoint::CKPT_HDR_LEN..]);
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
        }
        Ok(())
    }

    pub fn compact(&mut self) -> Result<(), Error> {
        crate::gc::compact_one(self)
    }
}
