//! Exhaustive Reed-Solomon RS(12,8) erasure characterisation.
//!
//! Enumerates *every* subset of block indices of size `e` for `e` in `0..=5`
//! (1 + 12 + 66 + 220 + 495 + 792 = 1586 patterns) against a stripe whose eight
//! data blocks hold real AEAD-sealed SLATE records, and classifies the outcome
//! of [`reconstruct`] into one of three buckets:
//!
//! * `recovered_exact` — returned `Ok` **and** all 12 blocks match the encoded
//!   stripe byte-for-byte **and** every sealed record in the reassembled data
//!   region re-opens under its AEAD key.
//! * `rejected` — returned `Err(TooManyErasures)`.
//! * `wrong_bytes` — returned `Ok` but at least one block differs. This is the
//!   dangerous outcome (silent data corruption presented as success) and the
//!   count must be zero.
//!
//! Two independent facts are measured alongside the classification:
//!
//! * `n_singular`: the public `reconstruct` signature collapses a singular
//!   survivor matrix into the same `TooManyErasures` error as an over-budget
//!   erasure count, so the survivor matrix is rebuilt here from the public
//!   [`cauchy_row`] / [`gf_matrix_invert`] and inverted directly. For an MDS
//!   code no `e <= RS_M` pattern may be singular.
//! * `mode = undeclared_corruption`: blocks are corrupted but *not* declared in
//!   the `BlockSet`. Erasure coding has no way to locate an undeclared error, so
//!   this measures what the RS layer does — not what it should do — and the
//!   result belongs in the paper as a scope limit of the parity layer.
//!
//! Emits CSV on stdout. Run with:
//!   cargo run --release -p slate-kv-sim --example rs_exhaustive

use slate_kv_core::config::{OP_PUT, REC_HDR_LEN, REC_OVERHEAD};
use slate_kv_core::log::{HeadState, Log, Sealer};
use slate_kv_core::record::RecordHeader;
use slate_kv_crypto::keys::{DeviceKey, KeySet};
use slate_kv_crypto::sealer::CryptoSealer;
use slate_kv_erasure::matrix::{cauchy_row, gf_matrix_invert};
use slate_kv_erasure::reconstruct::{reconstruct, BlockSet};
use slate_kv_erasure::{gf, PAGE_SIZE, RS_K, RS_M, RS_N};

/// Number of records the builder attempts to write into the stripe.
const N_RECORDS: usize = 40;
/// Bytes of the stripe that hold data (as opposed to parity).
const DATA_BYTES: usize = RS_K * PAGE_SIZE;

/// Builds a `RS_N`-block stripe whose data blocks are packed with real
/// AEAD-sealed SLATE records, then appends the Cauchy parity blocks.
///
/// Returns the encoded stripe and the byte length of the record region, so the
/// verifier knows where the 0xFF padding starts.
fn build_stripe(sealer: &mut CryptoSealer) -> ([[u8; PAGE_SIZE]; RS_N], usize) {
    let mut log_buf = [0u8; 8192];
    let mut chain = slate_kv_core::chain::Chain::anchor(1, &[0u8; 32]);
    let mut log = Log::<'_, slate_kv_sim::SimFlash>::new(
        &mut log_buf,
        HeadState {
            seg_seq: 1,
            write_offset: 0,
            block_idx: 0,
            ..Default::default()
        },
    );

    // Vary key and value length so records straddle page boundaries rather than
    // lining up with them; a stripe whose records never cross a block seam would
    // not exercise reconstruction of a partially-held record.
    let mut n_written = 0usize;
    for i in 0..N_RECORDS {
        let key = format!("cfg/sensor/{i:04}");
        let val = format!("reading={}", "9".repeat(1 + i % 17));
        // Stop before the record region overflows the data blocks.
        if log.batch.data().len() + REC_OVERHEAD + key.len() + val.len() > DATA_BYTES {
            break;
        }
        log.append(
            (i + 1) as u64,
            1,
            OP_PUT,
            key.as_bytes(),
            val.as_bytes(),
            sealer,
            &mut chain,
        )
        .expect("append into stripe batch");
        n_written += 1;
    }
    assert!(
        n_written >= 20,
        "stripe should hold at least 20 sealed records, held {n_written}"
    );

    let data = log.batch.data();
    let data_len = data.len();
    assert!(
        data_len > (RS_K - 1) * PAGE_SIZE,
        "records must span all {RS_K} data blocks, only filled {data_len} bytes"
    );

    let mut stripe = [[0u8; PAGE_SIZE]; RS_N];
    for (i, block) in stripe.iter_mut().take(RS_K).enumerate() {
        let start = i * PAGE_SIZE;
        let end = core::cmp::min(start + PAGE_SIZE, data_len);
        if start < data_len {
            block[..end - start].copy_from_slice(&data[start..end]);
            if end - start < PAGE_SIZE {
                block[end - start..].fill(0xFF);
            }
        } else {
            block.fill(0xFF);
        }
    }

    for j in 0..RS_M {
        let p_idx = RS_K + j;
        let row = cauchy_row(j);
        for i in 0..RS_K {
            let c = row[i];
            let d_row = stripe[i];
            for (o, &d) in stripe[p_idx].iter_mut().zip(&d_row) {
                *o ^= gf::gf_mul(c, d);
            }
        }
    }

    (stripe, data_len)
}

/// Re-opens every sealed record in the reassembled data region under its AEAD
/// key, returning the number that authenticated. A byte-exact reconstruction
/// must yield the pristine count; anything less would mean the bytes matched but
/// the crypto did not, which is a contradiction worth surfacing.
fn count_openable(
    stripe: &[[u8; PAGE_SIZE]; RS_N],
    data_len: usize,
    s: &mut CryptoSealer,
) -> usize {
    let mut flat = [0u8; DATA_BYTES];
    for i in 0..RS_K {
        flat[i * PAGE_SIZE..(i + 1) * PAGE_SIZE].copy_from_slice(&stripe[i]);
    }

    let mut off = 0usize;
    let mut ok = 0usize;
    while off + REC_HDR_LEN <= data_len {
        let mut hdr_bytes = [0u8; REC_HDR_LEN];
        hdr_bytes.copy_from_slice(&flat[off..off + REC_HDR_LEN]);
        let hdr = match RecordHeader::decode(&hdr_bytes) {
            Ok(h) => h,
            Err(_) => break,
        };
        let total = REC_OVERHEAD + hdr.klen as usize + hdr.vlen as usize;
        if off + total > data_len {
            break;
        }
        let mut plain = [0u8; 2048];
        if s.open_record(
            &hdr_bytes,
            &flat[off + REC_HDR_LEN..off + total],
            &mut plain,
        )
        .is_ok()
        {
            ok += 1;
        }
        off += total;
    }
    ok
}

/// Rebuilds the `RS_K`x`RS_K` survivor matrix the same way `reconstruct` does
/// and reports whether it is singular over GF(2^8).
///
/// `reconstruct` maps a singular matrix onto `TooManyErasures`, so this is the
/// only way to tell the two rejection causes apart from outside the crate.
fn survivor_matrix_singular(erased: &BlockSet) -> bool {
    let mut a = [[0u8; RS_K]; RS_K];
    let mut row = 0;
    for i in 0..RS_N {
        if !erased.contains(i) {
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
    if row < RS_K {
        // Fewer than k survivors: not a singularity, an under-determined system.
        return false;
    }
    gf_matrix_invert(a).is_err()
}

/// Enumerates every `e`-subset of `0..RS_N` in lexicographic order.
fn for_each_subset(e: usize, mut f: impl FnMut(&[usize])) {
    if e == 0 {
        f(&[]);
        return;
    }
    if e > RS_N {
        return;
    }
    let mut idx: Vec<usize> = (0..e).collect();
    loop {
        f(&idx);
        // Advance to the next combination.
        let mut i = e;
        loop {
            if i == 0 {
                return;
            }
            i -= 1;
            if idx[i] != i + RS_N - e {
                break;
            }
            if i == 0 {
                return;
            }
        }
        idx[i] += 1;
        for j in i + 1..e {
            idx[j] = idx[j - 1] + 1;
        }
    }
}

#[derive(Default)]
struct Tally {
    n_patterns: usize,
    n_recovered_exact: usize,
    n_rejected: usize,
    n_wrong_bytes: usize,
    n_singular: usize,
    n_aead_open_failures: usize,
}

fn main() {
    let dev_key = DeviceKey([0x42; 32]);
    let mut sealer = CryptoSealer::new(KeySet::derive(&dev_key, 1));
    let (encoded, data_len) = build_stripe(&mut sealer);

    let mut verify_sealer = CryptoSealer::new(KeySet::derive(&dev_key, 1));
    let baseline_openable = count_openable(&encoded, data_len, &mut verify_sealer);
    assert!(
        baseline_openable > 0,
        "no record in the pristine stripe authenticated; the stripe builder is wrong"
    );

    println!(
        "# slate RS({RS_N},{RS_K}) exhaustive erasure characterisation, GF(2^8) Cauchy, PAGE_SIZE={PAGE_SIZE}"
    );
    println!("# command: cargo run --release -p slate-kv-sim --example rs_exhaustive");
    println!(
        "# stripe: {RS_K} data blocks packed with {baseline_openable} real AEAD-sealed SLATE \
         records ({data_len} bytes of record data), {RS_M} Cauchy parity blocks"
    );
    println!(
        "# mode=declared_erasure: erased blocks zeroed AND declared in the BlockSet. \
         mode=undeclared_corruption: blocks bit-flipped but BlockSet left empty."
    );
    println!(
        "# recovered_exact requires all {RS_N} blocks byte-identical to the encoded stripe AND \
         all {baseline_openable} records re-openable under AEAD."
    );
    println!(
        "# platform: pure-computation model harness (no flash device involved); \
         host macOS 26.5.2 arm64"
    );
    println!(
        "mode,e,n_patterns,n_recovered_exact,n_rejected,n_wrong_bytes,n_singular_survivor_matrix,n_aead_open_failures"
    );

    // --- declared erasures: the MDS claim ---
    for e in 0..=(RS_M + 1) {
        let mut t = Tally::default();
        for_each_subset(e, |subset| {
            t.n_patterns += 1;

            let mut erased = BlockSet::new();
            let mut stripe = encoded;
            for &idx in subset {
                erased.insert(idx);
                stripe[idx] = [0u8; PAGE_SIZE];
            }
            if survivor_matrix_singular(&erased) {
                t.n_singular += 1;
            }

            match reconstruct(&mut stripe, &erased) {
                Ok(()) => {
                    if stripe == encoded {
                        let n_ok = count_openable(&stripe, data_len, &mut verify_sealer);
                        if n_ok == baseline_openable {
                            t.n_recovered_exact += 1;
                        } else {
                            t.n_aead_open_failures += 1;
                        }
                    } else {
                        t.n_wrong_bytes += 1;
                    }
                }
                Err(_) => t.n_rejected += 1,
            }
        });
        println!(
            "declared_erasure,{e},{},{},{},{},{},{}",
            t.n_patterns,
            t.n_recovered_exact,
            t.n_rejected,
            t.n_wrong_bytes,
            t.n_singular,
            t.n_aead_open_failures
        );
    }

    // --- undeclared corruption: what the parity layer cannot do ---
    // Erasure codes locate lost blocks from an external erasure signal. With an
    // empty BlockSet there is nothing to solve for, so `reconstruct` is a no-op
    // and the corrupt bytes survive. Detection is the AEAD's job, not RS's, and
    // the last column measures whether the AEAD does that job.
    for e in 1..=2 {
        let mut t = Tally::default();
        for_each_subset(e, |subset| {
            t.n_patterns += 1;
            let mut stripe = encoded;
            for &idx in subset {
                stripe[idx][0] ^= 0x01;
            }
            let erased = BlockSet::new();
            match reconstruct(&mut stripe, &erased) {
                Ok(()) => {
                    if stripe == encoded {
                        t.n_recovered_exact += 1;
                    } else {
                        t.n_wrong_bytes += 1;
                    }
                }
                Err(_) => t.n_rejected += 1,
            }
            // Did the AEAD catch what RS could not?
            let n_ok = count_openable(&stripe, data_len, &mut verify_sealer);
            if n_ok < baseline_openable {
                t.n_aead_open_failures += 1;
            }
        });
        println!(
            "undeclared_corruption,{e},{},{},{},{},{},{}",
            t.n_patterns,
            t.n_recovered_exact,
            t.n_rejected,
            t.n_wrong_bytes,
            t.n_singular,
            t.n_aead_open_failures
        );
    }

    // --- space overhead, from the crate's own constants ---
    println!("# space_overhead: RS_K={RS_K} data blocks, RS_M={RS_M} parity blocks, RS_N={RS_N}");
    println!(
        "# space_overhead: parity_blocks/data_blocks={:.6}  stripe_bytes/data_bytes={:.6}  \
         parity_bytes={}  data_bytes={}",
        RS_M as f64 / RS_K as f64,
        RS_N as f64 / RS_K as f64,
        RS_M * PAGE_SIZE,
        RS_K * PAGE_SIZE
    );
}
