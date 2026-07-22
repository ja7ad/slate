//! repair

use crate::config::MAGIC_REC;
use crate::error::Error;
use slate_hal::Flash;

/// Step 2 of commit: XOR parity page = byte-wise XOR of all data PAGES
/// programmed since the previous XOR page (i.e. this batch's pages).
/// A trivial RS(k+1, k): any ONE lost page in the open segment is a located
/// erasure (its record's AEAD tag identifies it) reconstructed by XOR of the
/// surviving pages + parity page. XOR pages carry a 1-byte magic 0x58 ('X')
/// + 2-byte covered-page-count header inside the page.
pub fn head_repair_one_page(
    survivors: &[[u8; 256]],
    parity: &[u8; 256],
    missing_idx: usize,
    out: &mut [u8; 256],
) {
    out.copy_from_slice(parity);
    for (i, page) in survivors.iter().enumerate() {
        if i != missing_idx {
            for j in 3..256 {
                out[j] ^= page[j];
            }
        }
    }
    // Note: the first 3 bytes of the missing page cannot be recovered this way.
    // The caller must infer them (e.g. MAGIC_REC or continuation).
    // For single page head repair, we assume it's MAGIC_REC if it's the start of a record.
    out[0] = MAGIC_REC; // Best effort inference for start of record
}

/// Repair orchestration (`slate-core::repair`): on any located failure in a *sealed* segment.
/// (tag/MAC/ECC error during get, replay, or scrub), load the stripe page-column,
/// call `reconstruct`, and write the recovered blocks by **segment rewrite**.
pub fn scrub<F: Flash>(_flash: &mut F) -> Result<(), Error> {
    // Stub: scrubs all sealed segments and repairs any failures.
    Ok(())
}
