//! record
#![allow(missing_docs)]

use crate::config::*;
use crate::error::Error;

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
