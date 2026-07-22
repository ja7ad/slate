//! chain
#![allow(missing_docs)]

use crate::config::{CHI_LEN, EPOCH_ANCHOR_TAG};
use sha2::{Digest, Sha256};

#[derive(Clone, Default)]
pub struct Chain {
    pub chi: [u8; CHI_LEN],
}

impl Chain {
    /// Re-anchor at epoch open (§3.4): χ₀^(e) = H("slate/epoch" || le64(e) || D_ckpt(e)).
    pub fn anchor(e: u64, d_ckpt: &[u8; 32]) -> Self {
        let mut h = Sha256::new();
        h.update(EPOCH_ANCHOR_TAG);
        h.update(e.to_le_bytes());
        h.update(d_ckpt);
        Chain {
            chi: h.finalize().into(),
        }
    }

    /// O(1) per record (§3.4 property 1): χ ← H(χ || r).
    pub fn fold(&mut self, record_bytes: &[u8]) {
        let mut h = Sha256::new();
        h.update(self.chi);
        h.update(record_bytes);
        self.chi = h.finalize().into();
    }
}
