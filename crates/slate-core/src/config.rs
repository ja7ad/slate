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
pub const CM_LEN: usize = 81;
pub const ERASED_BYTE: u8 = 0xFF;
pub const MAX_KEY_LEN: usize = 256;
pub const MAX_VAL_LEN: usize = 1024;
pub const B_COMMIT: usize = 27;

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
