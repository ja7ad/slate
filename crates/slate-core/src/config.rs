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
pub const MAX_CKPT_LEN: u32 = 40960;

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
pub const EPOCH_ANCHOR_TAG: &[u8] = b"slate/epoch";

#[derive(Clone, Copy)]
pub struct SchedCfg {
    pub auto_b: bool,
    pub fixed_cost_uj: u64,
    pub holding_nj_per_op_s: u64,
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
    pub staleness_budget_ms: u32,
}

#[derive(Debug)]
pub enum ConfigError {
    CounterBudgetExceeded,
    CapacityTooSmall,
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
            if self.staleness_budget_ms == 0 {
                return Err(ConfigError::CapacityTooSmall); // Needs proper error
            }
            if self.sched.deadline_ms < self.staleness_budget_ms {
                return Err(ConfigError::CapacityTooSmall); // Needs proper error
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
                holding_nj_per_op_s: 1000,
                deadline_ms: 1000,
                b_min: 1,
                b_max: 128,
                b_commit: 27,
            },
            staleness_budget_ms: 1000,
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
