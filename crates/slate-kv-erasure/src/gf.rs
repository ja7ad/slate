//! gf

use crate::GF_POLY;

/// Exponential table, doubled to skip mod 255.
pub const GF_EXP: [u8; 512] = build_exp();
/// Logarithm table.
pub const GF_LOG: [u8; 256] = build_log();

const fn build_exp() -> [u8; 512] {
    let mut exp = [0u8; 512];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= GF_POLY;
        }
        i += 1;
    }
    let mut i = 255;
    while i < 512 {
        exp[i] = exp[i - 255];
        i += 1;
    }
    exp
}

const fn build_log() -> [u8; 256] {
    let mut log = [0u8; 256];
    let exp = build_exp();
    let mut i = 0;
    while i < 255 {
        log[exp[i] as usize] = i as u8;
        i += 1;
    }
    log
}

/// Multiply two field elements.
#[inline]
pub fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        GF_EXP[GF_LOG[a as usize] as usize + GF_LOG[b as usize] as usize]
    }
}

/// Inverse of a field element.
#[inline]
pub fn gf_inv(a: u8) -> u8 {
    debug_assert!(a != 0);
    GF_EXP[255 - GF_LOG[a as usize] as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ref_gf_mul(a: u8, b: u8) -> u8 {
        let mut p = 0u8;
        let mut a = a;
        let mut b = b;
        for _ in 0..8 {
            if b & 1 != 0 {
                p ^= a;
            }
            let hi = a & 0x80;
            a <<= 1;
            if hi != 0 {
                a ^= (GF_POLY & 0xFF) as u8;
            }
            b >>= 1;
        }
        p
    }

    #[test]
    fn test_gf_mul() {
        for a in 0..=255 {
            for b in 0..=255 {
                let fast = gf_mul(a, b);
                let slow = ref_gf_mul(a, b);
                assert_eq!(fast, slow, "{} * {}", a, b);
            }
        }
    }

    #[test]
    fn test_gf_inv() {
        for a in 1..=255 {
            let inv = gf_inv(a);
            assert_eq!(gf_mul(a, inv), 1);
        }
    }
}
