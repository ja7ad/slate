//! slate
#![allow(missing_docs)]

use crate::config::*;
use crate::error::Error;
use crate::gc::SegTable;
use crate::log::{Log, Sealer};
use crate::index::Index;
use crate::epoch::EngineState;
use slate_hal::Flash;

pub struct Slate<'a, F: Flash, S: Sealer> {
    pub flash: F,
    pub sealer: S,
    pub engine: EngineState,
    pub log_hot: Log<'a, F>,
    pub log_cold: Log<'a, F>,
    pub index: Index<'a>,
    pub segs: SegTable,
    pub ckpt_seg_seq: u64,
    pub sched: crate::sched::Scheduler,
    pub metrics: crate::metrics::Metrics,
}

impl<'a, F: Flash, S: Sealer> Slate<'a, F, S> {
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
        let mut rng = crate::index::XorShift64::new(42);
        let _ = self.index.upsert(key, new_off, &mut rng, |_| false); // Ignore kick for now
    }

    pub fn append_cold(&mut self, key: &[u8], val: &[u8], now_ms: u64) -> Result<u32, Error> {
        self.log_cold.append(OP_PUT, key, val, &mut self.sealer, &mut self.engine.chain)?;
        if self.sched.on_append(now_ms) {
            self.commit()?;
        }
        Ok(self.log_cold.head.write_offset) // Approximation of new_off
    }

    pub fn append_cold_tombstone(&mut self, key: &[u8], now_ms: u64) -> Result<(), Error> {
        self.log_cold.append(OP_DEL, key, &[], &mut self.sealer, &mut self.engine.chain)?;
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
        self.log_hot.commit(&mut self.flash, &mut self.sealer, &self.engine.chain)?;
        self.log_cold.commit(&mut self.flash, &mut self.sealer, &self.engine.chain)?;
        self.sched.on_commit();
        self.metrics.add_commit();
        Ok(())
    }

    pub fn compact(&mut self) -> Result<(), Error> {
        crate::gc::compact_one(self)
    }
}
