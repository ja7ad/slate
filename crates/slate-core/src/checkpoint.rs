//! checkpoint
#![allow(missing_docs)]

use crate::config::MAGIC_CKPT;
use crate::error::Error;

/// Size of the checkpoint header (AD for the AEAD).
pub const CKPT_HDR_LEN: usize = 36;

pub struct CheckpointHeader {
    pub magic: u8,
    pub format_version: u8,
    pub epoch: u64,
    pub seq: u64,
    pub seg_seq: u64,
    pub write_offset: u32,
    pub n_keys: u16,
    pub ct_len: u32,
}

impl CheckpointHeader {
    pub fn encode(&self, out: &mut [u8; CKPT_HDR_LEN]) {
        out[0] = self.magic;
        out[1] = self.format_version;
        out[2..10].copy_from_slice(&self.epoch.to_le_bytes());
        out[10..18].copy_from_slice(&self.seq.to_le_bytes());
        out[18..26].copy_from_slice(&self.seg_seq.to_le_bytes());
        out[26..30].copy_from_slice(&self.write_offset.to_le_bytes());
        out[30..32].copy_from_slice(&self.n_keys.to_le_bytes());
        out[32..36].copy_from_slice(&self.ct_len.to_le_bytes());
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < CKPT_HDR_LEN || bytes[0] != MAGIC_CKPT {
            return Err(Error::FormatError);
        }
        let epoch = u64::from_le_bytes(bytes[2..10].try_into().unwrap());
        let seq = u64::from_le_bytes(bytes[10..18].try_into().unwrap());
        let seg_seq = u64::from_le_bytes(bytes[18..26].try_into().unwrap());
        let write_offset = u32::from_le_bytes(bytes[26..30].try_into().unwrap());
        let n_keys = u16::from_le_bytes(bytes[30..32].try_into().unwrap());
        let ct_len = u32::from_le_bytes(bytes[32..36].try_into().unwrap());
        
        Ok(Self {
            magic: bytes[0],
            format_version: bytes[1],
            epoch,
            seq,
            seg_seq,
            write_offset,
            n_keys,
            ct_len,
        })
    }
}
