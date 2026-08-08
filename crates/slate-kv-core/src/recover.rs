//! recover

use crate::chain::Chain;
use crate::config::*;
use crate::error::Error;
use crate::log::Sealer;
use crate::record::RecordHeader;
use slate_kv_hal::Flash;

/// Information about recovery result.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecoverInfo {
    /// Max committed sequence up to.
    pub committed_upto: u64,
    /// Position of head after truncate.
    pub head_pos: u32,
    /// Committed records the tail scan AEAD-opened and handed to `apply`.
    ///
    /// This is the Θ in the "O(1) tip check plus O(Θ) replay" mount claim, so it
    /// is reported rather than inferred: without it a reader cannot tell a mount
    /// that replayed nothing from one that replayed a full epoch, and the two
    /// have very different costs.
    pub records_applied: u64,
    /// Bytes of log the tail scan walked, i.e. `head_pos - start_off`.
    pub scan_bytes: u32,
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

/// Returns whether the on-flash record at `off` decrypts to exactly `key`.
///
/// Used during index reconstruction to resolve fingerprint collisions: two
/// distinct live keys can share an `f`-bit fingerprint, so an index update must
/// confirm the exact key (Thm 4.2) before overwriting a slot. All replayed
/// records are already durable on flash at recovery time, so a flash read
/// suffices (no batch lookup needed).
pub fn record_key_eq<F: Flash, S: Sealer>(
    flash: &mut F,
    sealer: &mut S,
    off: u32,
    key: &[u8],
) -> bool {
    let mut hdr_bytes = [0u8; REC_HDR_LEN];
    if flash.read(off, &mut hdr_bytes).is_err() {
        return false;
    }
    let hdr = match RecordHeader::decode(&hdr_bytes) {
        Ok(h) => h,
        Err(_) => return false,
    };
    if hdr.klen as usize != key.len() {
        return false;
    }
    let total_len = crate::config::REC_OVERHEAD + hdr.klen as usize + hdr.vlen as usize;
    let mut rec_bytes = [0u8; crate::config::REC_OVERHEAD + MAX_KEY_LEN + MAX_VAL_LEN];
    if total_len > rec_bytes.len() || flash.read(off, &mut rec_bytes[..total_len]).is_err() {
        return false;
    }
    let mut scratch = [0u8; MAX_KEY_LEN + MAX_VAL_LEN];
    if sealer
        .open_record(&hdr_bytes, &rec_bytes[REC_HDR_LEN..total_len], &mut scratch)
        .is_err()
    {
        return false;
    }
    &scratch[..hdr.klen as usize] == key
}

fn finish_truncate(
    off: u32,
    committed_upto: u64,
    start_off: u32,
    records_applied: u64,
) -> RecoverInfo {
    RecoverInfo {
        committed_upto,
        head_pos: off,
        records_applied,
        scan_bytes: off.saturating_sub(start_off),
    }
}

/// Scans for segment headers.
pub fn scan_segment_headers<F: Flash>(
    flash: &mut F,
) -> Result<heapless::Vec<u32, { crate::config::MAX_SEGS }>, Error> {
    let mut segs: heapless::Vec<(u32, u64), { crate::config::MAX_SEGS }> = heapless::Vec::new();
    let mut off = 0;
    let total_len = flash.capacity();
    let mut hdr_buf = [0u8; crate::segment::SegmentHeader::LEN];

    while off + (crate::config::SEG_BYTES as u32) <= total_len {
        if flash.read(off, &mut hdr_buf).is_ok() && hdr_buf[0] == crate::config::MAGIC_SEG {
            if let Ok(hdr) = crate::segment::SegmentHeader::decode(&hdr_buf) {
                let _ = segs.push((off, hdr.seg_seq));
            }
        }
        off += crate::config::SEG_BYTES as u32;
    }

    segs.sort_unstable_by_key(|&(_, seq)| seq);

    let mut res = heapless::Vec::new();
    for &(addr, _) in &segs {
        let _ = res.push(addr);
    }
    if res.is_empty() {
        let _ = res.push(0);
    }
    Ok(res)
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
///
/// The `apply` callback is invoked once per committed record in `seq` order. It
/// receives `&mut F` and `&mut S` so the caller can read and decrypt *other*
/// records (e.g. to compare full keys when rebuilding the RAM index and
/// deduplicate colliding fingerprints); the borrows of `flash`/`s` handed to the
/// callback are the same ones `recover` holds, reborrowed for the call.
#[allow(clippy::collapsible_if)]
#[allow(clippy::single_match)]
#[allow(clippy::too_many_arguments)]
pub fn recover<F: Flash, S: Sealer>(
    flash: &mut F,
    s: &mut S,
    chain: &mut Chain,
    epoch: u64,
    start_off: u32,
    workspace: &mut RecoverWorkspace,
    apply: impl FnMut(&mut F, &mut S, u64, u32, u8, &[u8]),
) -> Result<RecoverInfo, Error> {
    // One flat span from the replay point to the end of the region: the
    // pre-wrap behaviour, kept for callers that have no segment table.
    let cap = flash.capacity();
    recover_spans(
        flash,
        s,
        chain,
        epoch,
        start_off,
        &[(start_off, cap)],
        workspace,
        apply,
    )
}

/// Tail replay across an explicit list of address spans, walked in the order
/// given.
///
/// Once the log head can wrap, "forward in the log" stops being the same as
/// "ascending address": the segment holding the newest records may sit at a
/// lower address than the one holding the oldest. Replaying by address then
/// reconstructs the index from superseded records and silently rolls the
/// database back. The caller passes spans in `seg_seq` order — the durable
/// allocation order recorded in the on-flash segment headers — so replay
/// follows the log rather than the address space.
///
/// Each span is `(start, end)` and is walked until its end or its first erased
/// page, whichever comes first; the scan then moves to the next span. Replay
/// finishes at the end of the last span.
#[allow(clippy::too_many_arguments)]
pub fn recover_spans<F: Flash, S: Sealer>(
    flash: &mut F,
    s: &mut S,
    chain: &mut Chain,
    epoch: u64,
    start_off: u32,
    spans: &[(u32, u32)],
    workspace: &mut RecoverWorkspace,
    mut apply: impl FnMut(&mut F, &mut S, u64, u32, u8, &[u8]),
) -> Result<RecoverInfo, Error> {
    let mut committed_upto = 0;
    let mut records_applied: u64 = 0;
    workspace.pending.count = 0;
    workspace.pending.last_seq = 0;
    let mut scratch_chain = chain.clone();

    let page_size = flash.page_size() as u32;
    // The segment model is not materialized on flash (no MAGIC_SEG headers are
    // ever written), so the log is one flat append region [start_off, capacity).
    // `start_off` is the first byte above the checkpoint region (see
    // `config::data_base_offset`); scanning stops at the first erased page.
    if spans.is_empty() {
        return Ok(finish_truncate(
            start_off,
            committed_upto,
            start_off,
            records_applied,
        ));
    }

    let mut span_idx = 0usize;
    let mut off = spans[0].0;
    let mut span_end = spans[0].1;

    // Where the log actually ends, as distinct from where the span walk
    // finishes. These differ: the walk runs to the end of the last span, but
    // the append head belongs at the first erased byte of the newest span that
    // holds data. Returning the span end instead would park the head past the
    // end of the region and every later commit would report `FlashFull`.
    let mut head_pos = off;

    {
        let mut buf = [0u8; 1];

        loop {
            // Move to the next span when this one is exhausted. Spans are in
            // allocation order, so this follows the log across a wrap rather
            // than across the address space.
            let mut exhausted = false;
            while off >= span_end {
                span_idx += 1;
                if span_idx >= spans.len() {
                    exhausted = true;
                    break;
                }
                off = spans[span_idx].0;
                span_end = spans[span_idx].1;
            }
            if exhausted {
                break;
            }

            if off >= flash.capacity() {
                break;
            }
            if flash.read(off, &mut buf).is_err() {
                break;
            }
            head_pos = off;

            match buf[0] {
                ERASED_BYTE => {
                    let rem = off % page_size;
                    if rem != 0 {
                        off += page_size - rem;
                    } else {
                        // An erased page at a page boundary ends this span's
                        // data. The append head belongs HERE — at the first
                        // erased byte of the newest span holding data — not at
                        // the end of the span walk. Continue with the next
                        // span: on a wrapped log the segment written after this
                        // one lives elsewhere, and stopping here would truncate
                        // the tail.
                        head_pos = off;
                        off = span_end;
                        continue;
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
                                && f.epoch >= epoch
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
                                                        flash,
                                                        s,
                                                        seq,
                                                        apply_off,
                                                        hdr.op,
                                                        &workspace.scratch[..hdr.klen as usize],
                                                    );
                                                    records_applied += 1;
                                                }
                                                // If open_record fails here despite
                                                // the commit marker's chain hash
                                                // matching (which cryptographically
                                                // binds every record in the batch),
                                                // the read was transiently corrupt.
                                                // Skip the record rather than
                                                // panicking: boot must never abort
                                                // the device with an unwind.
                                            }
                                        }
                                    }
                                }
                                committed_upto = f.seq_max;
                            } else {
                                return Ok(finish_truncate(
                                    off,
                                    committed_upto,
                                    start_off,
                                    records_applied,
                                ));
                            }
                        }
                        Err(_) => {
                            return Ok(finish_truncate(
                                off,
                                committed_upto,
                                start_off,
                                records_applied,
                            ));
                        }
                    }
                    off += page_size * 2;
                }
                MAGIC_XOR => {
                    let rem = off % page_size;
                    if rem != 0 {
                        off += page_size - rem;
                    } else {
                        off += page_size;
                    }
                }
                _ => {
                    if buf[0] == MAGIC_REC {
                        let mut hdr_bytes = [0u8; REC_HDR_LEN];
                        if flash.read(off, &mut hdr_bytes).is_err() {
                            return Ok(finish_truncate(
                                off,
                                committed_upto,
                                start_off,
                                records_applied,
                            ));
                        }
                        if let Ok(hdr) = RecordHeader::decode(&hdr_bytes) {
                            let total_len =
                                crate::config::REC_OVERHEAD + hdr.klen as usize + hdr.vlen as usize;
                            if flash
                                .read(off, &mut workspace.rec_bytes[..total_len])
                                .is_err()
                            {
                                return Ok(finish_truncate(
                                    off,
                                    committed_upto,
                                    start_off,
                                    records_applied,
                                ));
                            }

                            match s.open_record(
                                &hdr_bytes,
                                &workspace.rec_bytes[REC_HDR_LEN..total_len],
                                &mut workspace.scratch,
                            ) {
                                Ok(()) => {
                                    scratch_chain.fold(&workspace.rec_bytes[..total_len]);
                                    if workspace.pending.push(hdr.seq, off).is_err() {
                                        return Ok(finish_truncate(
                                            off,
                                            committed_upto,
                                            start_off,
                                            records_applied,
                                        ));
                                    }
                                    off += total_len as u32;
                                    continue;
                                }
                                Err(_) => {}
                            }
                        }
                    }

                    return Ok(finish_truncate(
                        off,
                        committed_upto,
                        start_off,
                        records_applied,
                    ));
                }
            }
        }
    }

    Ok(finish_truncate(
        head_pos,
        committed_upto,
        start_off,
        records_applied,
    ))
}
