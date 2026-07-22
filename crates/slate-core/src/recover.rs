//! recover

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
    records: [u64; 100], // Adjust size if B_COMMIT is larger
    count: usize,
    last_seq: u64,
}

impl PendingBatch {
    /// Creates a new pending batch.
    pub fn new() -> Self {
        Self {
            records: [0; 100],
            count: 0,
            last_seq: 0,
        }
    }

    /// Pushes a sequence number.
    pub fn push(&mut self, seq: u64) {
        if self.count < self.records.len() {
            self.records[self.count] = seq;
            self.count += 1;
        }
        self.last_seq = seq;
    }

    /// Returns last sequence number.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Drains the pending batch.
    pub fn drain(&mut self) -> &[u64] {
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

/// Recovers the state from the flash log.
pub fn recover<F: Flash>(
    flash: &mut F,
    s: &mut impl Sealer,
    mut apply: impl FnMut(u64),
) -> Result<RecoverInfo, Error> {
    let segs = scan_segment_headers(flash)?;
    let mut committed_upto = 0;
    let mut pending = PendingBatch::new();

    let mut scratch = [0u8; MAX_KEY_LEN + MAX_VAL_LEN];
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
                MAGIC_REC => {
                    let mut hdr_bytes = [0u8; REC_HDR_LEN];
                    if flash.read(off, &mut hdr_bytes).is_err() {
                        return Ok(finish_truncate(off, committed_upto));
                    }
                    if let Ok(hdr) = RecordHeader::decode(&hdr_bytes) {
                        let total_len = 44 + hdr.klen as usize + hdr.vlen as usize;
                        let mut rec_bytes = [0u8; 44 + MAX_KEY_LEN + MAX_VAL_LEN];
                        if flash.read(off, &mut rec_bytes[..total_len]).is_err() {
                            return Ok(finish_truncate(off, committed_upto));
                        }

                        match s.open_record(
                            &hdr_bytes,
                            &rec_bytes[REC_HDR_LEN..total_len],
                            &mut scratch,
                        ) {
                            Ok(()) => {
                                s.chain_fold(&rec_bytes[..total_len]);
                                pending.push(hdr.seq);
                            }
                            Err(_) => {
                                return Ok(finish_truncate(off, committed_upto));
                            }
                        }
                        off += total_len as u32;
                    } else {
                        return Ok(finish_truncate(off, committed_upto));
                    }
                }
                MAGIC_CM => {
                    let mut cm1 = [0u8; CM_LEN];
                    let mut cm2 = [0u8; CM_LEN];
                    if flash.read(off, &mut cm1).is_err()
                        || flash.read(off + page_size, &mut cm2).is_err()
                    {
                        return Ok(finish_truncate(off, committed_upto));
                    }
                    let cm_valid = s.verify_marker(&cm1).or_else(|_| s.verify_marker(&cm2));
                    match cm_valid {
                        Ok(f) if f.seq_max == pending.last_seq() => {
                            let batch = pending.drain();
                            for &m in batch {
                                apply(m);
                            }
                            committed_upto = f.seq_max;
                        }
                        _ => {
                            return Ok(finish_truncate(off, committed_upto));
                        }
                    }
                    off += page_size * 2;
                }
                _ => {
                    return Ok(finish_truncate(off, committed_upto));
                }
            }
        }
    }

    Ok(finish_truncate(segs[0], committed_upto))
}
