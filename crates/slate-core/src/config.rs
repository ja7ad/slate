//! config
#![allow(missing_docs)]

pub const MAGIC_REC: u8 = 0xA5;
pub const MAGIC_CM: u8 = 0xC3;
pub const MAGIC_SEG: u8 = 0x51;
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
