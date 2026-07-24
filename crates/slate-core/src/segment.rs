//! segment

use crate::config::*;
use crate::error::Error;
use slate_erasure::gf::gf_mul;
use slate_erasure::matrix::cauchy_row;
use slate_erasure::{PAGE_SIZE, RS_K, RS_M};
use slate_hal::Flash;

/// Segment Header.
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

/// Represents a 12-block segment on flash.
pub struct Segment {
    /// Start address of the segment.
    pub start_addr: u32,
    /// Size of a single block.
    pub block_size: u32,
}

impl Segment {
    /// Returns address of the i-th data block.
    pub fn data_block(&self, i: usize) -> u32 {
        self.start_addr + (i as u32) * self.block_size
    }
    /// Returns address of the j-th parity block.
    pub fn parity_block(&self, j: usize) -> u32 {
        self.start_addr + ((RS_K + j) as u32) * self.block_size
    }
}

/// SEAL-TIME ENCODE (§7.2: once per sealed segment).
#[allow(clippy::needless_range_loop)]
pub fn encode_parity<F: Flash>(flash: &mut F, seg: &Segment) -> Result<(), Error> {
    let pages_per_block = (seg.block_size as usize) / PAGE_SIZE;

    // 1. Erase parity blocks
    for j in 0..RS_M {
        flash.erase(seg.parity_block(j)).map_err(|_| Error::Io)?;
    }

    // 2. Compute parity and write
    for page in 0..pages_per_block {
        let mut par = [[0u8; PAGE_SIZE]; RS_M];
        for i in 0..RS_K {
            let mut d = [0u8; PAGE_SIZE];
            flash
                .read(seg.data_block(i) + (page * PAGE_SIZE) as u32, &mut d)
                .map_err(|_| Error::Io)?;
            for j in 0..RS_M {
                let c = cauchy_row(j)[i];
                for (p, &db) in par[j].iter_mut().zip(&d) {
                    *p ^= gf_mul(c, db);
                }
            }
        }
        for j in 0..RS_M {
            flash
                .program(seg.parity_block(j) + (page * PAGE_SIZE) as u32, &par[j])
                .map_err(|_| Error::Io)?;
        }
    }
    Ok(())
}
