//! slate-erasure

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(missing_docs)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::clone_on_copy)]

pub mod gf;
pub mod matrix;
pub mod reconstruct;

/// Number of data blocks in a stripe (k).
pub const RS_K: usize = 8;
/// Number of parity blocks in a stripe (m).
pub const RS_M: usize = 4;
/// Total number of blocks in a stripe (n).
pub const RS_N: usize = 12;

/// GF(2^8) polynomial `x^8 + x^4 + x^3 + x^2 + 1`.
pub const GF_POLY: u16 = 0x11D;
/// Generator for log/exp tables.
pub const GF_GEN: u8 = 0x02;

/// Page size used for reconstruction buffer dimension.
pub const PAGE_SIZE: usize = 256;

#[derive(Debug, PartialEq, Eq)]
pub struct TooManyErasures;

#[derive(Debug, PartialEq, Eq)]
pub struct Singular;
