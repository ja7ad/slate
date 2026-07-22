//! reconstruct

use crate::gf::gf_mul;
use crate::matrix::{cauchy_row, gf_matrix_invert};
use crate::{PAGE_SIZE, RS_K, RS_M, RS_N, TooManyErasures};

/// Bitset of erased blocks.
#[derive(Clone, Default)]
pub struct BlockSet(u16);

impl BlockSet {
    /// Creates an empty set.
    pub fn new() -> Self {
        Self(0)
    }
    /// Adds a block index to the set.
    pub fn insert(&mut self, idx: usize) {
        if idx < RS_N {
            self.0 |= 1 << idx;
        }
    }
    /// Returns true if the set contains the index.
    pub fn contains(&self, idx: usize) -> bool {
        if idx < RS_N {
            (self.0 & (1 << idx)) != 0
        } else {
            false
        }
    }
    /// Returns the number of erased blocks.
    pub fn count(&self) -> usize {
        self.0.count_ones() as usize
    }
}

fn survivor_matrix(erased: &BlockSet) -> ([usize; RS_K], [[u8; RS_K]; RS_K]) {
    let mut surv_idx = [0usize; RS_K];
    let mut a = [[0u8; RS_K]; RS_K];
    let mut row = 0;

    for i in 0..RS_N {
        if !erased.contains(i) {
            surv_idx[row] = i;
            if i < RS_K {
                a[row][i] = 1;
            } else {
                a[row] = cauchy_row(i - RS_K);
            }
            row += 1;
            if row == RS_K {
                break;
            }
        }
    }

    (surv_idx, a)
}

/// ERASURE RECONSTRUCTION (§7).
pub fn reconstruct(
    stripe_pages: &mut [[u8; PAGE_SIZE]; RS_N],
    erased: &BlockSet,
) -> Result<(), TooManyErasures> {
    if erased.count() > RS_M {
        return Err(TooManyErasures);
    }

    if erased.count() == 0 {
        return Ok(());
    }

    let (surv_idx, a) = survivor_matrix(erased);
    // Unreachable singular because Cauchy matrix properties guarantee invertibility.
    let ainv = gf_matrix_invert(a).map_err(|_| TooManyErasures)?;

    for d in 0..RS_K {
        if !erased.contains(d) {
            continue;
        }
        let mut out = [0u8; PAGE_SIZE];
        for r in 0..RS_K {
            let c = ainv[d][r];
            if c == 0 {
                continue;
            }
            for (o, &s) in out.iter_mut().zip(&stripe_pages[surv_idx[r]]) {
                *o ^= gf_mul(c, s);
            }
        }
        stripe_pages[d] = out;
    }

    // Re-encode erased parity blocks
    for j in 0..RS_M {
        let p_idx = RS_K + j;
        if erased.contains(p_idx) {
            let mut out = [0u8; PAGE_SIZE];
            let row = cauchy_row(j);
            for i in 0..RS_K {
                let c = row[i];
                for (o, &d) in out.iter_mut().zip(&stripe_pages[i]) {
                    *o ^= gf_mul(c, d);
                }
            }
            stripe_pages[p_idx] = out;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exhaustive_erasure_patterns() {
        // Build a deterministic stripe
        let mut original_stripe = [[0u8; PAGE_SIZE]; RS_N];
        for i in 0..RS_K {
            for j in 0..PAGE_SIZE {
                original_stripe[i][j] = ((i * 13 + j) % 256) as u8;
            }
        }

        // Encode parity
        for j in 0..RS_M {
            let p_idx = RS_K + j;
            let row = cauchy_row(j);
            for i in 0..RS_K {
                let c = row[i];
                let d_row = original_stripe[i]; // copy to avoid borrow conflict
                for (o, &d) in original_stripe[p_idx].iter_mut().zip(&d_row) {
                    *o ^= gf_mul(c, d);
                }
            }
        }

        let mut tests_run = 0;

        // Try all combinations of up to RS_M erasures out of RS_N
        for count in 1..=RS_M {
            let mut combination = [0usize; RS_N];
            for i in 0..count {
                combination[i] = 1;
            }
            combination.sort(); // start from [0,0,..,1,1]

            loop {
                // Test this combination
                let mut erased = BlockSet::new();
                for i in 0..RS_N {
                    if combination[i] == 1 {
                        erased.insert(i);
                    }
                }

                let mut test_stripe = original_stripe.clone();
                for i in 0..RS_N {
                    if erased.contains(i) {
                        test_stripe[i] = [0u8; PAGE_SIZE]; // Wipe
                    }
                }

                reconstruct(&mut test_stripe, &erased).unwrap();

                for i in 0..RS_N {
                    assert_eq!(
                        test_stripe[i], original_stripe[i],
                        "Mismatch at block {}",
                        i
                    );
                }

                tests_run += 1;

                // Next permutation
                let mut i = RS_N - 1;
                while i > 0 && combination[i - 1] >= combination[i] {
                    i -= 1;
                }
                if i == 0 {
                    break;
                }
                let mut j = RS_N - 1;
                while combination[j] <= combination[i - 1] {
                    j -= 1;
                }
                combination.swap(i - 1, j);
                combination[i..].reverse();
            }
        }

        // C(12,1) + C(12,2) + C(12,3) + C(12,4) = 12 + 66 + 220 + 495 = 793
        assert_eq!(tests_run, 793);
    }
}
