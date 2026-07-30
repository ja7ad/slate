//! config
#![allow(missing_docs)]

/// Magic byte for a record header.
pub const MAGIC_REC: u8 = 0x5A;
/// Magic byte for a commit marker.
pub const MAGIC_CM: u8 = 0x5C;
/// Magic byte for a segment header.
pub const MAGIC_SEG: u8 = 0x51;
/// Magic byte for XOR head page.
pub const MAGIC_XOR: u8 = 0x58;
pub const OP_PUT: u8 = 0x00;
pub const OP_DEL: u8 = 0x01;
pub const REC_HDR_LEN: usize = 28;
pub const TAG_LEN: usize = 16;
pub const REC_OVERHEAD: usize = REC_HDR_LEN + TAG_LEN;
pub const SEG_BLOCKS_DATA: usize = 8;
pub const SEG_BLOCKS_PARITY: usize = 4;
pub const SEG_BYTES: usize = 49_152;
pub const CM_LEN: usize = 83;
pub const ERASED_BYTE: u8 = 0xFF;
pub const MAX_KEY_LEN: usize = 256;
pub const MAX_VAL_LEN: usize = 1024;
pub const B_COMMIT: usize = 27;
pub const B_MAX: usize = 128;
pub const MAX_PAGE_SIZE: usize = 512;
/// Largest index the checkpoint format can hold, in slots.
///
/// The entire index is serialized into a single checkpoint, which is what makes
/// mount cost O(Θ) instead of O(log length): the index is loaded, not rebuilt.
/// That ties index capacity to checkpoint capacity. 65 536 slots is the table
/// the default `n_keys = 8192` configuration allocates (8192/α rounded to a
/// power of two, times `BUCKET_SLOTS`).
pub const MAX_INDEX_SLOTS: usize = 65_536;

/// Maximum size of one serialized checkpoint slot, header and AEAD tag
/// included.
///
/// A *format* constant: it fixes how much flash each of the `CKPT_SLOTS`
/// checkpoint slots reserves (see [`data_base_offset`]), so changing it
/// invalidates existing volumes. Derived from [`MAX_INDEX_SLOTS`] rather than
/// chosen independently, so the two can never drift apart.
pub const MAX_CKPT_LEN: u32 = ckpt_len_for_slots(MAX_INDEX_SLOTS) as u32;
pub const MAX_SEGS: usize = 256;
/// Yield cadence inside the tail-replay scan (§4.5)
pub const RECOVER_YIELD_EVERY_PAGES: usize = 32;

/// Yield cadence inside the compact_one scan loop (§4.4)
pub const GC_YIELD_EVERY_RECORDS: u16 = 8;

// Index constants (ESP32 default 8 k-key config)
pub const BUCKET_SLOTS: usize = 4;
pub const STASH_SIZE: usize = 8;
pub const FP_BITS: usize = 8;
pub const OFF_BITS: usize = 24;
pub const MAX_KICKS: usize = 500;
pub const N_BUCKETS: usize = 2048;

// Epoch & Checkpoint constants
pub const THETA: usize = 16384;
pub const CHI_LEN: usize = 32;
pub const MAGIC_CKPT: u8 = 0xCF;
pub const CKPT_SLOTS: usize = 2;
pub const CKPT_BASE_BLOCK: u32 = 2;
pub const EPOCH_ANCHOR_TAG: &[u8] = b"slate/epoch";
/// Size of the checkpoint header (the AEAD associated data).
/// Re-exported as `checkpoint::CKPT_HDR_LEN`; it lives here so the capacity
/// helpers below can be `const` without a module cycle.
pub const CKPT_HDR_LEN: usize = 76;

/// Serialized size of an index with `n_slots` slots, as written by
/// `Index::serialize` (4 bytes per slot plus a 5-byte stash entry each).
pub const fn index_serialized_len(n_slots: usize) -> usize {
    n_slots * 4 + STASH_SIZE * 5
}

/// Total checkpoint bytes needed for an index with `n_slots` slots: header,
/// serialized index, and the 16-byte AEAD tag.
pub const fn ckpt_len_for_slots(n_slots: usize) -> usize {
    CKPT_HDR_LEN + index_serialized_len(n_slots) + 16
}

/// Largest index (in slots) whose checkpoint still fits in [`MAX_CKPT_LEN`].
///
/// Callers that size an index from a user-supplied key count must check against
/// this rather than discovering the overflow as a failed `commit` — by then the
/// epoch seal is already unable to make progress, which silently disables
/// checkpointing for the life of the database.
pub const fn max_index_slots() -> usize {
    MAX_INDEX_SLOTS
}

const _CKPT_HOLDS_MAX_INDEX: () = assert!(
    ckpt_len_for_slots(MAX_INDEX_SLOTS) <= MAX_CKPT_LEN as usize,
    "MAX_CKPT_LEN cannot hold MAX_INDEX_SLOTS"
);

/// First flash byte the append log may use. The log grows upward from here and
/// must never reach the checkpoint region (blocks `CKPT_BASE_BLOCK ..
/// CKPT_BASE_BLOCK + CKPT_SLOTS * ceil(MAX_CKPT_LEN / block_size)`), or a commit
/// would try to program the still-live checkpoint pages and fail
/// `ProgramWithoutErase`. Computed from the runtime block size so it is correct
/// for both the 4 KiB file/ESP32 blocks and any other geometry.
/// Erase blocks reserved for one checkpoint slot.
///
/// Must round *up*: a slot that needs 64.03 blocks needs 65, and truncating to
/// 64 would place slot 1 one block inside slot 0. That is not a slow path, it
/// is silent destruction of the very redundancy the two-slot scheme exists for
/// — programming the new checkpoint erases the block holding the tail of the
/// old one, so a crash mid-seal leaves neither slot readable and the volume
/// unmountable. Every site that addresses a slot must use this function rather
/// than recomputing the division.
#[inline]
pub fn ckpt_blocks_per_slot(block_size: usize) -> u32 {
    MAX_CKPT_LEN.div_ceil(block_size as u32)
}

/// Byte address of checkpoint slot `slot`.
#[inline]
pub fn ckpt_slot_addr(slot: u8, block_size: usize) -> u32 {
    let bs = block_size as u32;
    (CKPT_BASE_BLOCK + slot as u32 * ckpt_blocks_per_slot(block_size)) * bs
}

/// First byte of the append log: immediately above all checkpoint slots.
pub fn data_base_offset(block_size: usize) -> u32 {
    let bs = block_size as u32;
    (CKPT_BASE_BLOCK + CKPT_SLOTS as u32 * ckpt_blocks_per_slot(block_size)) * bs
}

#[cfg(test)]
mod ckpt_layout_tests {
    use super::*;

    /// Checkpoint slots must not overlap each other or the log.
    ///
    /// The two-slot scheme is the only thing standing between a crash during
    /// `program_checkpoint` and an unmountable volume: the previous checkpoint
    /// has to stay intact while the new one is written. Computing
    /// `blocks_per_slot` with truncating division instead of `div_ceil` puts
    /// slot 1 inside slot 0 whenever `MAX_CKPT_LEN` is not a block multiple,
    /// which erases the old checkpoint at exactly the moment it is needed.
    #[test]
    fn ckpt_slots_do_not_overlap() {
        for &bs in &[256usize, 4096, 65_536] {
            let span = ckpt_blocks_per_slot(bs) as usize * bs;
            assert!(
                span >= MAX_CKPT_LEN as usize,
                "block_size {bs}: slot span {span} < MAX_CKPT_LEN {MAX_CKPT_LEN}"
            );
            for slot in 1..CKPT_SLOTS as u8 {
                let prev_end = ckpt_slot_addr(slot - 1, bs) as usize + MAX_CKPT_LEN as usize;
                assert!(
                    ckpt_slot_addr(slot, bs) as usize >= prev_end,
                    "block_size {bs}: slot {slot} overlaps slot {}",
                    slot - 1
                );
            }
            let last_end =
                ckpt_slot_addr(CKPT_SLOTS as u8 - 1, bs) as usize + MAX_CKPT_LEN as usize;
            assert!(
                data_base_offset(bs) as usize >= last_end,
                "block_size {bs}: log base {} overlaps the last checkpoint slot",
                data_base_offset(bs)
            );
        }
    }
}

#[derive(Clone, Copy)]
pub struct SchedCfg {
    pub auto_b: bool,
    pub fixed_cost_uj: u64,
    pub staleness_budget_ms: u32,
    pub deadline_ms: u32,
    pub b_min: u32,
    pub b_max: u32,
    pub b_commit: u32,
}

#[derive(Clone, Copy)]
pub struct SlateConfig {
    pub arena_bytes: usize,
    pub expected_live_bytes: usize,
    pub counter_budget: u64,
    pub expected_life_ops: u64,
    pub sched: SchedCfg,
}

#[derive(Debug)]
pub enum ConfigError {
    CounterBudgetExceeded,
    CapacityTooSmall,
    InvalidSchedCfg,
}

impl SlateConfig {
    pub fn validate(&mut self) -> Result<(), ConfigError> {
        if self.arena_bytes < 2 * self.expected_live_bytes {
            return Err(ConfigError::CapacityTooSmall);
        }

        let required_counter = self.expected_life_ops / (THETA as u64);
        if self.counter_budget < required_counter.max(1) {
            return Err(ConfigError::CounterBudgetExceeded);
        }

        if self.sched.auto_b {
            if self.sched.staleness_budget_ms == 0 {
                return Err(ConfigError::InvalidSchedCfg);
            }
            if self.sched.deadline_ms < self.sched.staleness_budget_ms {
                return Err(ConfigError::InvalidSchedCfg);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_validation() {
        let mut cfg = SlateConfig {
            arena_bytes: 4096 * 10,
            expected_live_bytes: 4096 * 4,
            counter_budget: 1000,
            expected_life_ops: 1000 * THETA as u64,
            sched: SchedCfg {
                auto_b: true,
                fixed_cost_uj: 400,
                staleness_budget_ms: 1000,
                deadline_ms: 1000,
                b_min: 1,
                b_max: 128,
                b_commit: 27,
            },
        };
        assert!(cfg.validate().is_ok());

        cfg.counter_budget = 999;
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::CounterBudgetExceeded)
        ));

        cfg.counter_budget = 1000;
        cfg.expected_live_bytes = 4096 * 6;
        assert!(matches!(cfg.validate(), Err(ConfigError::CapacityTooSmall)));
    }
}
