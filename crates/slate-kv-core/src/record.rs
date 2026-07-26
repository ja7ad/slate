//! record
#![allow(missing_docs)]

use crate::config::*;
use crate::error::Error;

/// Byte range of the sequence number inside the 96-bit record nonce.
pub const NONCE_SEQ_RANGE: core::ops::Range<usize> = 0..8;
/// Byte range of the epoch discriminator inside the 96-bit record nonce.
pub const NONCE_EPOCH_RANGE: core::ops::Range<usize> = 8..12;
/// Byte range of the nonce inside the encoded record header.
pub const HDR_NONCE_RANGE: core::ops::Range<usize> = 16..28;

/// Builds the record nonce (§3.3).
///
/// The low 8 bytes are the sequence number, which alone already guarantees
/// uniqueness because `seq` is a strictly increasing total order that is never
/// reset. The high 4 bytes carry the **epoch discriminator**: the record key
/// `k_rec_e` is rotated every epoch, so a reader must know which epoch a record
/// was sealed under in order to derive the right key. Stamping the epoch here
/// costs no extra header bytes and — because the header is the AEAD associated
/// data — is authenticated, so a flipped epoch surfaces as `Tampered` rather
/// than as silently wrong plaintext.
#[inline]
pub fn record_nonce(seq: u64, epoch: u32) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[NONCE_SEQ_RANGE].copy_from_slice(&seq.to_le_bytes());
    n[NONCE_EPOCH_RANGE].copy_from_slice(&epoch.to_le_bytes());
    n
}

/// Reads the epoch discriminator back out of a record nonce.
#[inline]
pub fn nonce_epoch(nonce: &[u8; 12]) -> u32 {
    u32::from_le_bytes(nonce[NONCE_EPOCH_RANGE].try_into().unwrap())
}

/// Reads the epoch discriminator directly from an encoded record header.
#[inline]
pub fn hdr_epoch(hdr: &[u8; REC_HDR_LEN]) -> u32 {
    let nonce: &[u8; 12] = hdr[HDR_NONCE_RANGE].try_into().unwrap();
    nonce_epoch(nonce)
}

/// Largest epoch representable in the record nonce. `seal_epoch` refuses to
/// roll past this rather than silently aliasing epoch keys.
pub const MAX_REC_EPOCH: u64 = u32::MAX as u64;

pub struct RecordHeader {
    pub magic: u8,
    pub seq: u64,
    pub op: u8,
    pub fp: u16,
    pub klen: u16,
    pub vlen: u16,
    pub nonce: [u8; 12],
}

impl RecordHeader {
    /// The epoch this record was sealed under, read from the nonce.
    #[inline]
    pub fn epoch(&self) -> u32 {
        nonce_epoch(&self.nonce)
    }

    pub fn encode(&self, out: &mut [u8; REC_HDR_LEN]) {
        out[0] = self.magic;
        out[1..9].copy_from_slice(&self.seq.to_le_bytes());
        out[9] = self.op;
        out[10..12].copy_from_slice(&self.fp.to_le_bytes());
        out[12..14].copy_from_slice(&self.klen.to_le_bytes());
        out[14..16].copy_from_slice(&self.vlen.to_le_bytes());
        out[16..28].copy_from_slice(&self.nonce);
    }

    pub fn decode(buf: &[u8; REC_HDR_LEN]) -> Result<Self, Error> {
        if buf[0] != MAGIC_REC {
            return Err(Error::FormatError);
        }
        let seq = u64::from_le_bytes(buf[1..9].try_into().unwrap());
        let op = buf[9];
        let fp = u16::from_le_bytes(buf[10..12].try_into().unwrap());
        let klen = u16::from_le_bytes(buf[12..14].try_into().unwrap());
        let vlen = u16::from_le_bytes(buf[14..16].try_into().unwrap());
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&buf[16..28]);

        if op != OP_PUT && op != OP_DEL {
            return Err(Error::FormatError);
        }

        if klen as usize > MAX_KEY_LEN || vlen as usize > MAX_VAL_LEN {
            return Err(Error::FormatError);
        }

        Ok(Self {
            magic: buf[0],
            seq,
            op,
            fp,
            klen,
            vlen,
            nonce,
        })
    }
}
