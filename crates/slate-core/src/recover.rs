//! recover

use crate::chain::Chain;
use crate::config::*;
use crate::error::Error;
use crate::log::Sealer;
use crate::record::RecordHeader;
use slate_hal::Flash;

/// Information about recovery result.
pub struct RecoverInfo {
    /// Max committed sequence up to.
    pub committed_upto: u64,
    /// Position of head after truncate.
    pub head_pos: u32,
}

/// A batch of pending record sequence numbers.
pub struct PendingBatch {
    records: [(u64, u32); B_MAX],
    count: usize,
    last_seq: u64,
}

impl PendingBatch {
    /// Creates a new pending batch.
    pub fn new() -> Self {
        Self {
            records: [(0, 0); B_MAX],
            count: 0,
            last_seq: 0,
        }
    }

    /// Pushes a sequence number.
    pub fn push(&mut self, seq: u64, off: u32) -> Result<(), Error> {
        if self.count < self.records.len() {
            self.records[self.count] = (seq, off);
            self.count += 1;
            self.last_seq = seq;
            Ok(())
        } else {
            Err(Error::FormatError)
        }
    }

    /// Returns last sequence number.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Drains the pending batch.
    pub fn drain(&mut self) -> &[(u64, u32)] {
        let res = &self.records[0..self.count];
        self.count = 0;
        res
    }
}

impl Default for PendingBatch {
    fn default() -> Self {
        Self::new()
    }
}

fn finish_truncate(off: u32, committed_upto: u64) -> RecoverInfo {
    RecoverInfo {
        committed_upto,
        head_pos: off,
    }
}

/// Scans for segment headers.
pub fn scan_segment_headers<F: Flash>(_flash: &mut F) -> Result<[u32; 1], Error> {
    // Stub: return a single segment at 0 for now. Real implementation in doc 007.
    Ok([0])
}

/// Workspace for the recovery process to avoid large stack allocations.
pub struct RecoverWorkspace {
    /// Batch of pending sequence numbers.
    pub pending: PendingBatch,
    /// Scratch buffer for cryptographic operations.
    pub scratch: [u8; MAX_KEY_LEN + MAX_VAL_LEN],
    /// Buffer for reading record bytes.
    pub rec_bytes: [u8; crate::config::REC_OVERHEAD + MAX_KEY_LEN + MAX_VAL_LEN],
}

impl RecoverWorkspace {
    /// Creates a new recovery workspace.
    pub fn new() -> Self {
        Self {
            pending: PendingBatch::new(),
            scratch: [0u8; MAX_KEY_LEN + MAX_VAL_LEN],
            rec_bytes: [0u8; crate::config::REC_OVERHEAD + MAX_KEY_LEN + MAX_VAL_LEN],
        }
    }
}

impl Default for RecoverWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

/// Recovers the state from the flash log.
#[allow(clippy::collapsible_if)]
#[allow(clippy::single_match)]
pub fn recover<F: Flash>(
    flash: &mut F,
    s: &mut impl Sealer,
    chain: &mut Chain,
    epoch: u64,
    workspace: &mut RecoverWorkspace,
    mut apply: impl FnMut(u64, u32, u8, &[u8]),
) -> Result<RecoverInfo, Error> {
    let segs = scan_segment_headers(flash)?;
    let mut committed_upto = 0;
    workspace.pending.count = 0;
    workspace.pending.last_seq = 0;
    let mut scratch_chain = chain.clone();

    let page_size = flash.page_size() as u32;

    for &seg_addr in &segs {
        let mut off = seg_addr + page_size; // Skip segment header
        let mut buf = [0u8; 1];

        loop {
            if off >= flash.capacity() {
                break;
            }
            if flash.read(off, &mut buf).is_err() {
                break;
            }

            match buf[0] {
                ERASED_BYTE => {
                    let rem = off % page_size;
                    if rem != 0 {
                        off += page_size - rem;
                    } else {
                        break;
                    }
                }
                MAGIC_CM => {
                    let mut cm1 = [0u8; CM_LEN];
                    let mut cm2 = [0u8; CM_LEN];
                    let r1 = flash.read(off, &mut cm1);
                    let r2 = flash.read(off + page_size, &mut cm2);

                    let mut cm_valid = Err(Error::FormatError);
                    if r1.is_ok() {
                        cm_valid = s.verify_marker(&cm1);
                    }
                    if cm_valid.is_err() && r2.is_ok() {
                        cm_valid = s.verify_marker(&cm2);
                    }
                    match cm_valid {
                        Ok(f) => {
                            if f.seq_max == workspace.pending.last_seq()
                                && f.chi == scratch_chain.chi
                                && f.epoch == epoch
                            {
                                *chain = scratch_chain.clone();
                                let batch = workspace.pending.drain();
                                for &(seq, apply_off) in batch {
                                    let mut hdr_bytes = [0u8; REC_HDR_LEN];
                                    if flash.read(apply_off, &mut hdr_bytes).is_ok() {
                                        if let Ok(hdr) = RecordHeader::decode(&hdr_bytes) {
                                            let total_len = crate::config::REC_OVERHEAD
                                                + hdr.klen as usize
                                                + hdr.vlen as usize;
                                            if flash
                                                .read(
                                                    apply_off,
                                                    &mut workspace.rec_bytes[..total_len],
                                                )
                                                .is_ok()
                                            {
                                                if s.open_record(
                                                    &hdr_bytes,
                                                    &workspace.rec_bytes[REC_HDR_LEN..total_len],
                                                    &mut workspace.scratch,
                                                )
                                                .is_ok()
                                                {
                                                    apply(
                                                        seq,
                                                        apply_off,
                                                        hdr.op,
                                                        &workspace.scratch[..hdr.klen as usize],
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                committed_upto = f.seq_max;
                            } else {
                                return Ok(finish_truncate(off, committed_upto));
                            }
                        }
                        Err(_) => {
                            return Ok(finish_truncate(off, committed_upto));
                        }
                    }
                    off += page_size * 2;
                }
                _ => {
                    // Before parsing, if we are at a page boundary, check if this is the XOR page
                    let rem = off % page_size;
                    if rem == 0 {
                        let mut next_cm = [0u8; CM_LEN];
                        if flash.read(off + page_size, &mut next_cm).is_ok()
                            && next_cm[0] == MAGIC_CM
                        {
                            if s.verify_marker(&next_cm).is_ok() {
                                // This page is the XOR parity page. Skip it.
                                off += page_size;
                                continue;
                            }
                        }
                    }

                    if buf[0] == MAGIC_REC {
                        let mut hdr_bytes = [0u8; REC_HDR_LEN];
                        if flash.read(off, &mut hdr_bytes).is_err() {
                            return Ok(finish_truncate(off, committed_upto));
                        }
                        if let Ok(hdr) = RecordHeader::decode(&hdr_bytes) {
                            let total_len =
                                crate::config::REC_OVERHEAD + hdr.klen as usize + hdr.vlen as usize;
                            if flash
                                .read(off, &mut workspace.rec_bytes[..total_len])
                                .is_err()
                            {
                                return Ok(finish_truncate(off, committed_upto));
                            }

                            match s.open_record(
                                &hdr_bytes,
                                &workspace.rec_bytes[REC_HDR_LEN..total_len],
                                &mut workspace.scratch,
                            ) {
                                Ok(()) => {
                                    scratch_chain.fold(&workspace.rec_bytes[..total_len]);
                                    if workspace.pending.push(hdr.seq, off).is_err() {
                                        return Ok(finish_truncate(off, committed_upto));
                                    }
                                    off += total_len as u32;
                                    continue;
                                }
                                Err(_) => {}
                            }
                        }
                    }

                    return Ok(finish_truncate(off, committed_upto));
                }
            }
        }
    }

    Ok(finish_truncate(segs[0], committed_upto))
}
