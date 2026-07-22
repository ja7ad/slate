//! index

use crate::config::*;
use crate::error::Error;

/// Buffer for holding candidate offsets.
pub struct CandidateBuf {
    offsets: [u32; 2 * BUCKET_SLOTS + STASH_SIZE],
    count: usize,
}

impl CandidateBuf {
    /// Creates a new candidate buffer.
    pub fn new() -> Self {
        Self {
            offsets: [0; 2 * BUCKET_SLOTS + STASH_SIZE],
            count: 0,
        }
    }

    /// Pushes an offset into the buffer.
    pub fn push(&mut self, off: u32) {
        if self.count < self.offsets.len() {
            self.offsets[self.count] = off;
            self.count += 1;
        }
    }

    /// Returns the active slice of offsets.
    pub fn as_slice(&self) -> &[u32] {
        &self.offsets[..self.count]
    }
}

impl Default for CandidateBuf {
    fn default() -> Self {
        Self::new()
    }
}

/// Deterministic PRNG for victim choice.
pub struct XorShift64(pub u64);

impl XorShift64 {
    /// Creates a new PRNG.
    pub fn new(seed: u64) -> Self {
        Self(if seed == 0 { 1 } else { seed })
    }

    /// Returns next random value.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// Returns true/false.
    pub fn bit(&mut self) -> bool {
        (self.next_u64() & 1) == 1
    }

    /// Returns a value in 0..range.
    pub fn next_range(&mut self, range: usize) -> usize {
        (self.next_u64() % (range as u64)) as usize
    }
}

/// 64-bit FNV-1a hash.
pub fn h64(key: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in key {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Returns a fingerprint for a key (never 0).
#[inline]
pub fn fingerprint(key: &[u8]) -> u8 {
    let f = (h64(key) >> 56) as u8;
    if f == 0 { 1 } else { f }
}

/// Returns the primary bucket index.
#[inline]
pub fn bucket1(h: u64, n: usize) -> usize {
    (h as usize) & (n - 1)
}

/// Returns the alternate bucket index.
#[inline]
pub fn alt_bucket(i: usize, fp: u8, n: usize) -> usize {
    i ^ ((fp as usize).wrapping_mul(0x5bd1e995) & (n - 1))
}

#[inline]
fn pack(fp: u8, off: u32) -> u32 {
    debug_assert!(off < (1 << OFF_BITS));
    ((fp as u32) << 24) | (off & 0x00FF_FFFF)
}

/// The index structure.
pub struct Index<'a> {
    slots: &'a mut [u32],
    n_buckets: usize,
    stash: [(u8, u32); STASH_SIZE],
    len: usize,
}

impl<'a> Index<'a> {
    /// Creates a new index.
    pub fn new(slots: &'a mut [u32], n_buckets: usize) -> Self {
        debug_assert!(n_buckets.is_power_of_two());
        debug_assert_eq!(slots.len(), n_buckets * BUCKET_SLOTS);
        slots.fill(0);
        Self {
            slots,
            n_buckets,
            stash: [(0, 0); STASH_SIZE],
            len: 0,
        }
    }

    /// Returns number of items.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Is index empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn bucket_mut(&mut self, i: usize) -> &mut [u32] {
        let start = i * BUCKET_SLOTS;
        &mut self.slots[start..start + BUCKET_SLOTS]
    }

    fn bucket(&self, i: usize) -> &[u32] {
        let start = i * BUCKET_SLOTS;
        &self.slots[start..start + BUCKET_SLOTS]
    }

    /// Lookup key and push candidates to buffer.
    pub fn candidates(&self, key: &[u8], out: &mut CandidateBuf) {
        let h = h64(key);
        let fp = fingerprint(key);
        let n = self.n_buckets;
        let i1 = bucket1(h, n);
        let i2 = alt_bucket(i1, fp, n);

        for &i in &[i1, i2] {
            for &s in self.bucket(i) {
                if (s >> 24) as u8 == fp {
                    out.push(s & 0x00FF_FFFF);
                }
            }
        }
        for &(sfp, soff) in &self.stash {
            if sfp == fp {
                out.push(soff & 0x00FF_FFFF);
            }
        }
    }

    /// Insert or update an entry.
    pub fn upsert(
        &mut self,
        key: &[u8],
        off: u32,
        rng: &mut XorShift64,
        mut matches_key: impl FnMut(u32) -> bool,
    ) -> Result<(), Error> {
        let h = h64(key);
        let fp = fingerprint(key);
        let n = self.n_buckets;
        let i1 = bucket1(h, n);
        let i2 = alt_bucket(i1, fp, n);

        // (1) update in place
        for &i in &[i1, i2] {
            let mut found_idx = None;
            for (idx, &s) in self.bucket(i).iter().enumerate() {
                if (s >> 24) as u8 == fp && matches_key(s & 0x00FF_FFFF) {
                    found_idx = Some(idx);
                    break;
                }
            }
            if let Some(idx) = found_idx {
                self.bucket_mut(i)[idx] = pack(fp, off);
                return Ok(());
            }
        }

        for e in &mut self.stash {
            if e.0 == fp && matches_key(e.1) {
                e.1 = off;
                return Ok(());
            }
        }

        // (2) insert into any empty slot
        for &i in &[i1, i2] {
            for s in self.bucket_mut(i) {
                if (*s >> 24) == 0 {
                    *s = pack(fp, off);
                    self.len += 1;
                    return Ok(());
                }
            }
        }

        // (3) random-walk relocation
        let mut cur_fp = fp;
        let mut cur_off = off;
        let mut i = if rng.bit() { i1 } else { i2 };

        for _ in 0..MAX_KICKS {
            let victim = rng.next_range(BUCKET_SLOTS);
            let s = &mut self.bucket_mut(i)[victim];
            let vfp = (*s >> 24) as u8;
            let voff = *s & 0x00FF_FFFF;
            *s = pack(cur_fp, cur_off);
            cur_fp = vfp;
            cur_off = voff;
            i = alt_bucket(i, cur_fp, n);

            for s in self.bucket_mut(i) {
                if (*s >> 24) == 0 {
                    *s = pack(cur_fp, cur_off);
                    self.len += 1;
                    return Ok(());
                }
            }
        }

        // (4) stash absorbs
        for e in &mut self.stash {
            if e.0 == 0 {
                *e = (cur_fp, cur_off);
                self.len += 1;
                return Ok(());
            }
        }

        Err(Error::IndexFull)
    }

    /// Remove a key.
    pub fn remove(&mut self, key: &[u8], mut matches_key: impl FnMut(u32) -> bool) -> bool {
        let h = h64(key);
        let fp = fingerprint(key);
        let n = self.n_buckets;
        let i1 = bucket1(h, n);
        let i2 = alt_bucket(i1, fp, n);

        for &i in &[i1, i2] {
            for s in self.bucket_mut(i) {
                if (*s >> 24) as u8 == fp && matches_key(*s & 0x00FF_FFFF) {
                    *s = 0; // Clear
                    self.len -= 1;
                    return true;
                }
            }
        }

        for e in &mut self.stash {
            if e.0 == fp && matches_key(e.1) {
                *e = (0, 0);
                self.len -= 1;
                return true;
            }
        }

        false
    }
}

#[cfg(test)]
extern crate std;

// Static assert for table size (32 KB slots + stash)
const _TABLE_SIZE_ASSERT: () = assert!(
    (N_BUCKETS * BUCKET_SLOTS * 4) + (STASH_SIZE * 6) <= 33000,
    "Table size exceeds budget"
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;

    #[test]
    fn test_involution() {
        let n = N_BUCKETS;
        for fp in 1..=255 {
            for i in 0..n {
                let alt = alt_bucket(i, fp, n);
                let alt2 = alt_bucket(alt, fp, n);
                assert_eq!(i, alt2, "Involution failed for fp={} i={}", fp, i);
            }
        }
    }

    #[test]
    fn test_pack_unpack() {
        let fp = 42;
        let off = 0x123456;
        let p = pack(fp, off);
        assert_eq!((p >> 24) as u8, fp);
        assert_eq!(p & 0x00FF_FFFF, off);
    }

    #[test]
    fn test_upsert_lookup_remove() {
        let mut slots = vec![0u32; N_BUCKETS * BUCKET_SLOTS];
        let mut index = Index::new(&mut slots, N_BUCKETS);
        let mut rng = XorShift64::new(42);

        let key = b"testkey";
        let off = 100;

        index.upsert(key, off, &mut rng, |_| false).unwrap();

        let mut cbuf = CandidateBuf::new();
        index.candidates(key, &mut cbuf);
        assert!(cbuf.as_slice().contains(&off));

        // Update
        index.upsert(key, 200, &mut rng, |o| o == off).unwrap();
        let mut cbuf = CandidateBuf::new();
        index.candidates(key, &mut cbuf);
        assert!(!cbuf.as_slice().contains(&off));
        assert!(cbuf.as_slice().contains(&200));

        // Remove
        let removed = index.remove(key, |o| o == 200);
        assert!(removed);

        let mut cbuf = CandidateBuf::new();
        index.candidates(key, &mut cbuf);
        assert!(!cbuf.as_slice().contains(&200));
    }

    #[test]
    fn test_load_factor() {
        let n_keys = (N_BUCKETS as f64 * BUCKET_SLOTS as f64 * 0.95) as usize;
        let mut slots = vec![0u32; N_BUCKETS * BUCKET_SLOTS];
        let mut index = Index::new(&mut slots, N_BUCKETS);
        let mut rng = XorShift64::new(42);

        for i in 0..n_keys {
            let key = i.to_le_bytes();
            // Just assume no collisions in our mock matches_key initially
            index
                .upsert(&key, i as u32 + 1, &mut rng, |_| false)
                .expect("Index should not be full at alpha=0.95");
        }

        // Verify all lookups
        for i in 0..n_keys {
            let key = i.to_le_bytes();
            let mut cbuf = CandidateBuf::new();
            index.candidates(&key, &mut cbuf);
            assert!(cbuf.as_slice().contains(&(i as u32 + 1)));
        }
    }
}
