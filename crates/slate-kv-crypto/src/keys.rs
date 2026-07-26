//! keys

use core::fmt;
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::ZeroizeOnDrop;

/// Raw device key. Never on flash, never Debug/Display/serialized.
#[derive(ZeroizeOnDrop)]
pub struct DeviceKey(pub [u8; 32]);

impl fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceKey(<REDACTED>)")
    }
}

#[derive(ZeroizeOnDrop)]
pub struct KeySet {
    pub prk: [u8; 32],     // HKDF-Extract(salt=HKDF_SALT, ikm=K)
    pub k_cm: [u8; 32],    // Expand(prk, "cm")
    pub k_ckpt: [u8; 32],  // Expand(prk, "ckpt")
    pub k_rec_e: [u8; 32], // Expand(prk, "rec"||le64(e)) — CURRENT epoch only
    pub k_ctr: [u8; 32],   // Expand(prk, "ctr")
    pub epoch: u64,
}

impl fmt::Debug for KeySet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeySet(<REDACTED>)")
    }
}

// Helper to safely expand keys.
fn expand(prk: &[u8; 32], info: &[u8], out: &mut [u8; 32]) {
    let hk = Hkdf::<Sha256>::from_prk(prk).expect("valid prk");
    hk.expand(info, out).expect("valid length");
}

impl KeySet {
    /// Derive a new KeySet from a DeviceKey and epoch.
    pub fn derive(k: &DeviceKey, epoch: u64) -> Self {
        let (prk, _) = Hkdf::<Sha256>::extract(Some(b"SLATE/v1"), &k.0);
        let mut ks = KeySet {
            prk: prk.into(),
            k_cm: [0; 32],
            k_ckpt: [0; 32],
            k_rec_e: [0; 32],
            k_ctr: [0; 32],
            epoch: 0,
        };
        expand(&ks.prk, b"cm", &mut ks.k_cm);
        expand(&ks.prk, b"ckpt", &mut ks.k_ckpt);
        expand(&ks.prk, b"ctr", &mut ks.k_ctr);
        ks.roll_epoch(epoch);
        ks
    }

    /// One KDF call per epoch: fresh record subkey, nonce space reset.
    pub fn roll_epoch(&mut self, e: u64) {
        let mut k = [0u8; 32];
        self.derive_rec_key(e, &mut k);
        self.k_rec_e = k;
        self.epoch = e;
    }

    /// Derives the record subkey for an arbitrary epoch into `out`.
    ///
    /// Reading a record sealed in an older epoch needs that epoch's key, not the
    /// currently open one, so this is exposed separately from [`Self::roll_epoch`].
    /// The KDF is deterministic, so an old key can always be re-derived from the
    /// device key without ever storing it.
    pub fn derive_rec_key(&self, e: u64, out: &mut [u8; 32]) {
        let mut info = [0u8; 3 + 8];
        info[..3].copy_from_slice(b"rec");
        info[3..].copy_from_slice(&e.to_le_bytes());
        expand(&self.prk, &info, out);
    }
}

/// Nonce (§3.3): `seq` in the low 64 bits, epoch discriminator in the high 32.
///
/// Re-exported from `slate_kv_core::record` so the layout has exactly one
/// definition shared by the core codec and the crypto seam.
pub use slate_kv_core::record::record_nonce;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_nonce() {
        let n1 = record_nonce(1, 7);
        let n2 = record_nonce(2, 7);
        assert_ne!(n1, n2);
        assert_eq!(n1[0..8], 1u64.to_le_bytes());
        assert_eq!(n1[8..12], 7u32.to_le_bytes());
        // Same seq in a different epoch is still a distinct nonce.
        assert_ne!(record_nonce(1, 7), record_nonce(1, 8));
    }

    #[test]
    fn old_epoch_key_is_rederivable() {
        let dk = DeviceKey([9; 32]);
        let mut ks = KeySet::derive(&dk, 3);
        let k_at_3 = ks.k_rec_e;
        ks.roll_epoch(4);
        assert_ne!(ks.k_rec_e, k_at_3);
        // Re-deriving epoch 3 from the same device key reproduces it exactly:
        // this is what lets open_record read pre-rotation records.
        let mut back = [0u8; 32];
        ks.derive_rec_key(3, &mut back);
        assert_eq!(back, k_at_3);
    }

    #[test]
    fn test_keyset_derivation() {
        let dk = DeviceKey([42; 32]);
        let mut ks = KeySet::derive(&dk, 1);

        // Ensure k_cm, k_ckpt, k_rec_e are derived
        assert_ne!(ks.k_cm, [0; 32]);
        assert_ne!(ks.k_ckpt, [0; 32]);
        assert_ne!(ks.k_rec_e, [0; 32]);

        // Roll epoch
        let old_k_rec_e = ks.k_rec_e;
        ks.roll_epoch(2);
        assert_ne!(ks.k_rec_e, old_k_rec_e);
        assert_eq!(ks.epoch, 2);
    }
}
