//! matrix

use crate::gf::{gf_inv, gf_mul};
use crate::{RS_K, Singular};

/// Parity matrix P: RS_M×RS_K Cauchy matrix, P[j][i] = 1/(x_j ⊕ y_i) with
/// x_j = (RS_K + j) as u8, y_i = i as u8.
pub fn cauchy_row(j: usize) -> [u8; RS_K] {
    core::array::from_fn(|i| gf_inv(((RS_K + j) as u8) ^ (i as u8)))
}

#[allow(clippy::needless_range_loop)]
fn identity() -> [[u8; RS_K]; RS_K] {
    let mut id = [[0u8; RS_K]; RS_K];
    for i in 0..RS_K {
        id[i][i] = 1;
    }
    id
}

/// Gauss–Jordan over GF(2⁸), in place, k=8.
pub fn gf_matrix_invert(mut a: [[u8; RS_K]; RS_K]) -> Result<[[u8; RS_K]; RS_K], Singular> {
    let mut inv = identity();
    for col in 0..RS_K {
        let piv = (col..RS_K).find(|&r| a[r][col] != 0).ok_or(Singular)?;
        a.swap(col, piv);
        inv.swap(col, piv);
        let pinv = gf_inv(a[col][col]);
        for c in 0..RS_K {
            a[col][c] = gf_mul(a[col][c], pinv);
            inv[col][c] = gf_mul(inv[col][c], pinv);
        }
        for r in 0..RS_K {
            if r != col && a[r][col] != 0 {
                let f = a[r][col];
                for c in 0..RS_K {
                    a[r][c] ^= gf_mul(f, a[col][c]);
                    inv[r][c] ^= gf_mul(f, inv[col][c]);
                }
            }
        }
    }
    Ok(inv)
}
