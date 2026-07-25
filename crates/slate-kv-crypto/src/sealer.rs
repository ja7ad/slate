//! sealer
#![allow(deprecated)]

use crate::keys::KeySet;
use chacha20poly1305::{
    aead::{AeadInPlace, KeyInit},
    ChaCha20Poly1305,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use slate_kv_core::config::{CM_LEN, REC_HDR_LEN};
use slate_kv_core::error::Error;
use slate_kv_core::log::{CmFields, Sealer};
use zeroize::Zeroize;

pub struct CryptoSealer {
    keys: KeySet,
}

impl CryptoSealer {
    pub fn new(keys: KeySet) -> Self {
        Self { keys }
    }
}

impl Sealer for CryptoSealer {
    fn seal_record(&mut self, hdr: &[u8; REC_HDR_LEN], kv: &[u8], out: &mut [u8]) {
        let cipher = ChaCha20Poly1305::new((&self.keys.k_rec_e).into());
        let nonce: &[u8; 12] = (&hdr[16..28]).try_into().unwrap();
        out[..kv.len()].copy_from_slice(kv);
        let tag = cipher
            .encrypt_in_place_detached(nonce.into(), hdr, &mut out[..kv.len()])
            .expect("length checked");
        out[kv.len()..].copy_from_slice(&tag);
    }

    fn open_record(
        &mut self,
        hdr: &[u8; REC_HDR_LEN],
        ct_tag: &[u8],
        out: &mut [u8],
    ) -> Result<(), Error> {
        let cipher = ChaCha20Poly1305::new((&self.keys.k_rec_e).into());
        let (ct, tag) = ct_tag.split_at(ct_tag.len() - 16);
        out[..ct.len()].copy_from_slice(ct);
        let nonce: &[u8; 12] = (&hdr[16..28]).try_into().unwrap();
        let tag_arr: &[u8; 16] = tag.try_into().unwrap();
        cipher
            .decrypt_in_place_detached(nonce.into(), hdr, &mut out[..ct.len()], tag_arr.into())
            .map_err(|_| {
                out.zeroize();
                Error::Tampered
            })
    }

    fn commit_marker(
        &mut self,
        seq_max: u64,
        epoch: u64,
        xor_pages: u16,
        chi: &[u8; 32],
    ) -> [u8; CM_LEN] {
        let mut cm = [0u8; CM_LEN];
        cm[0] = slate_kv_core::config::MAGIC_CM;
        cm[1..9].copy_from_slice(&seq_max.to_le_bytes());
        cm[9..17].copy_from_slice(&epoch.to_le_bytes());
        cm[17..19].copy_from_slice(&xor_pages.to_le_bytes());
        cm[19..51].copy_from_slice(chi);

        let mut mac = Hmac::<Sha256>::new_from_slice(&self.keys.k_cm).expect("valid key length");
        mac.update(&cm[0..51]);
        let tau = mac.finalize().into_bytes();
        cm[51..83].copy_from_slice(&tau);
        cm
    }

    fn verify_marker(&self, cm: &[u8; CM_LEN]) -> Result<CmFields, Error> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.keys.k_cm).expect("valid key length");
        mac.update(&cm[0..51]);
        if mac.verify_slice(&cm[51..83]).is_err() {
            return Err(Error::Tampered);
        }
        if cm[0] != slate_kv_core::config::MAGIC_CM {
            return Err(Error::FormatError);
        }

        let mut seq_max_bytes = [0u8; 8];
        seq_max_bytes.copy_from_slice(&cm[1..9]);
        let seq_max = u64::from_le_bytes(seq_max_bytes);

        let mut epoch_bytes = [0u8; 8];
        epoch_bytes.copy_from_slice(&cm[9..17]);
        let epoch = u64::from_le_bytes(epoch_bytes);

        let mut xor_pages_bytes = [0u8; 2];
        xor_pages_bytes.copy_from_slice(&cm[17..19]);
        let xor_pages = u16::from_le_bytes(xor_pages_bytes);

        let mut chi = [0u8; 32];
        chi.copy_from_slice(&cm[19..51]);

        let mut tau_cm = [0u8; 32];
        tau_cm.copy_from_slice(&cm[51..83]);

        Ok(CmFields {
            magic: cm[0],
            seq_max,
            epoch,
            xor_pages,
            chi,
            tau_cm,
        })
    }

    fn seal_checkpoint(&mut self, epoch: u64, slot: u8, ad: &[u8], in_out: &mut [u8]) -> [u8; 16] {
        let cipher = ChaCha20Poly1305::new((&self.keys.k_ckpt).into());
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&epoch.to_le_bytes());
        nonce[8] = slot;

        let tag = cipher
            .encrypt_in_place_detached(&nonce.into(), ad, in_out)
            .expect("length checked");
        let mut t = [0u8; 16];
        t.copy_from_slice(&tag);
        t
    }

    fn open_checkpoint(
        &mut self,
        epoch: u64,
        slot: u8,
        ad: &[u8],
        in_out: &mut [u8],
        tag: &[u8; 16],
    ) -> Result<(), Error> {
        let cipher = ChaCha20Poly1305::new((&self.keys.k_ckpt).into());
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&epoch.to_le_bytes());
        nonce[8] = slot;

        cipher
            .decrypt_in_place_detached(&nonce.into(), ad, in_out, tag.into())
            .map_err(|_| {
                in_out.zeroize();
                Error::Tampered
            })
    }

    fn roll_epoch(&mut self, e: u64) {
        self.keys.roll_epoch(e);
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::DeviceKey;
    use slate_kv_core::config::MAGIC_REC;
    use std::vec;

    fn setup_sealer() -> CryptoSealer {
        let dk = DeviceKey([42; 32]);
        let ks = KeySet::derive(&dk, 1);
        CryptoSealer::new(ks)
    }

    #[test]
    fn test_marker_mac() {
        let mut sealer = setup_sealer();
        let cm = sealer.commit_marker(100, 1, 1, &[0; 32]);
        let fields = sealer.verify_marker(&cm).unwrap();
        assert_eq!(fields.seq_max, 100);
        assert_eq!(fields.epoch, 1);
        assert_eq!(fields.xor_pages, 1);
    }

    #[test]
    fn test_marker_tamper() {
        let mut sealer = setup_sealer();
        let mut cm = sealer.commit_marker(100, 1, 1, &[0; 32]);
        cm[5] ^= 1; // Flip bit in seq_max
        assert!(matches!(sealer.verify_marker(&cm), Err(Error::Tampered)));
    }

    #[test]
    fn test_record_seal_open() {
        let mut sealer = setup_sealer();
        let mut hdr = [0u8; REC_HDR_LEN];
        hdr[0] = MAGIC_REC;
        hdr[1..9].copy_from_slice(&100u64.to_le_bytes()); // seq=100
        hdr[16..24].copy_from_slice(&100u64.to_le_bytes()); // nonce

        let kv = b"hello_world";
        let mut ct_tag = vec![0u8; kv.len() + 16];
        sealer.seal_record(&hdr, kv, &mut ct_tag);

        let mut out = vec![0u8; kv.len()];
        sealer.open_record(&hdr, &ct_tag, &mut out).unwrap();
        assert_eq!(&out, kv);
    }

    #[test]
    fn test_record_tamper_ct() {
        let mut sealer = setup_sealer();
        let mut hdr = [0u8; REC_HDR_LEN];
        hdr[0] = MAGIC_REC;
        hdr[1..9].copy_from_slice(&100u64.to_le_bytes());
        hdr[16..24].copy_from_slice(&100u64.to_le_bytes());

        let kv = b"hello_world";
        let mut ct_tag = vec![0u8; kv.len() + 16];
        sealer.seal_record(&hdr, kv, &mut ct_tag);

        ct_tag[0] ^= 1; // Tamper ciphertext

        let mut out = vec![0u8; kv.len()];
        assert!(matches!(
            sealer.open_record(&hdr, &ct_tag, &mut out),
            Err(Error::Tampered)
        ));
        assert_eq!(out, vec![0u8; kv.len()]); // zeroized
    }

    #[test]
    fn test_record_tamper_hdr() {
        let mut sealer = setup_sealer();
        let mut hdr = [0u8; REC_HDR_LEN];
        hdr[0] = MAGIC_REC;
        hdr[1..9].copy_from_slice(&100u64.to_le_bytes());
        hdr[16..24].copy_from_slice(&100u64.to_le_bytes());

        let kv = b"hello_world";
        let mut ct_tag = vec![0u8; kv.len() + 16];
        sealer.seal_record(&hdr, kv, &mut ct_tag);

        hdr[5] ^= 1; // Tamper header (AD)

        let mut out = vec![0u8; kv.len()];
        assert!(matches!(
            sealer.open_record(&hdr, &ct_tag, &mut out),
            Err(Error::Tampered)
        ));
    }
}
