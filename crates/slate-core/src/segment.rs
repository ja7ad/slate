//! segment

use crate::config::*;
use crate::error::Error;

/// Segment Header as per design doc 002.
#[derive(Debug, Clone, Copy)]
pub struct SegmentHeader {
    /// Magic byte (0x51).
    pub magic: u8,
    /// Format version.
    pub format_version: u8,
    /// Segment allocation number.
    pub seg_seq: u64,
    /// Epoch when opened.
    pub epoch: u64,
    /// Lowest record seq in segment.
    pub minseq: u64,
    /// 0xFF if open, 0x00 if sealed.
    pub sealed: u8,
    /// HMAC over bytes 0..27.
    pub hdr_mac: [u8; 32],
}

impl SegmentHeader {
    /// Length of segment header.
    pub const LEN: usize = 59;

    /// Encodes header into a buffer.
    pub fn encode(&self, out: &mut [u8; Self::LEN]) {
        out[0] = self.magic;
        out[1] = self.format_version;
        out[2..10].copy_from_slice(&self.seg_seq.to_le_bytes());
        out[10..18].copy_from_slice(&self.epoch.to_le_bytes());
        out[18..26].copy_from_slice(&self.minseq.to_le_bytes());
        out[26] = self.sealed;
        out[27..59].copy_from_slice(&self.hdr_mac);
    }

    /// Decodes header from a buffer.
    pub fn decode(buf: &[u8; Self::LEN]) -> Result<Self, Error> {
        if buf[0] != MAGIC_SEG {
            return Err(Error::FormatError);
        }
        Ok(Self {
            magic: buf[0],
            format_version: buf[1],
            seg_seq: u64::from_le_bytes(buf[2..10].try_into().unwrap()),
            epoch: u64::from_le_bytes(buf[10..18].try_into().unwrap()),
            minseq: u64::from_le_bytes(buf[18..26].try_into().unwrap()),
            sealed: buf[26],
            hdr_mac: buf[27..59].try_into().unwrap(),
        })
    }
}
