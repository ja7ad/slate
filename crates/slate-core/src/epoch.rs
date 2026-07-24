//! epoch
#![allow(missing_docs)]

use crate::chain::Chain;
use crate::checkpoint::{CheckpointHeader, CKPT_HDR_LEN};
use crate::config::CKPT_SLOTS;
use crate::error::Error;
use crate::log::Sealer;
use sha2::{Digest, Sha256};
use slate_hal::{CounterKind, Flash, MonotonicCounter};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SecurityMode {
    Full,
    BestEffortRollback,
    NoRollbackProtection,
}

#[derive(Debug)]
pub enum MountError {
    Rollback,
    Tampered,
    Io,
    FormatError,
}

impl From<Error> for MountError {
    fn from(e: Error) -> Self {
        match e {
            Error::Tampered => MountError::Tampered,
            Error::FormatError => MountError::FormatError,
            Error::Io => MountError::Io,
            _ => MountError::Io,
        }
    }
}

pub struct EngineState {
    pub epoch: u64,
    pub next_seq: u64,
    pub acked_seq: u64,
    pub d_ckpt: [u8; 32],
    pub chain: Chain,
    pub records_in_epoch: usize,
    pub security_mode: SecurityMode,
    pub active_ckpt_slot: u8,
}

impl EngineState {
    pub fn next_ckpt_slot(&self) -> u8 {
        (self.active_ckpt_slot + 1) % (CKPT_SLOTS as u8)
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn program_checkpoint<F: Flash>(flash: &mut F, slot: u8, bytes: &[u8]) -> Result<(), Error> {
    // Checkpoint area is after superblock.
    // Each slot requires MAX_CKPT_LEN / block_size blocks.
    let blocks_per_slot = crate::config::MAX_CKPT_LEN / flash.block_size() as u32;
    let block_addr = (crate::config::CKPT_BASE_BLOCK + slot as u32 * blocks_per_slot) * flash.block_size() as u32;
    
    // Erase enough blocks for the serialized checkpoint
    let num_blocks = bytes.len().div_ceil(flash.block_size());
    for i in 0..num_blocks {
        flash.erase(block_addr + (i * flash.block_size()) as u32).map_err(|_| Error::Io)?;
    }
    // Pad bytes to page size
    let page_size = flash.page_size();
    let num_pages = bytes.len().div_ceil(page_size);
    let mut page_buf = [0xFF; 256]; // Assuming 256 is the max page size for now, or pass from caller

    for i in 0..num_pages {
        let start = i * page_size;
        let end = core::cmp::min(start + page_size, bytes.len());
        page_buf[..end - start].copy_from_slice(&bytes[start..end]);
        if end - start < page_size {
            page_buf[end - start..page_size].fill(0xFF);
        }
        flash
            .program(block_addr + start as u32, &page_buf[..page_size])
            .map_err(|_| Error::Io)?;
    }
    Ok(())
}

/// EPOCH SEAL — the write-ahead protocol.
#[allow(clippy::too_many_arguments)]
pub fn seal_epoch<F: Flash, C: MonotonicCounter>(
    st: &mut EngineState,
    flash: &mut F,
    ctr: &mut C,
    s: &mut impl Sealer,
    seg_seq: u64,
    write_offset: u32,
    n_keys: u16,
    ckpt_buf: &mut [u8],
    index_len: usize,
) -> Result<(), Error> {
    let e = st.epoch;

    // 1. write + flush checkpoint carrying counter field e
    let slot = st.next_ckpt_slot();

    let hdr = CheckpointHeader {
        magic: crate::config::MAGIC_CKPT,
        format_version: 1,
        epoch: e,
        seq: st.next_seq,
        seg_seq,
        write_offset,
        n_keys,
        ct_len: index_len as u32 + 16,
        chi: st.chain.chi,
        mc: e,
    };

    let total_len = CKPT_HDR_LEN + index_len + 16;
    if ckpt_buf.len() < total_len {
        return Err(Error::FormatError);
    }

    let mut hdr_bytes = [0u8; CKPT_HDR_LEN];
    hdr.encode(&mut hdr_bytes);
    ckpt_buf[..CKPT_HDR_LEN].copy_from_slice(&hdr_bytes);

    let tag = s.seal_checkpoint(
        e,
        slot,
        &hdr_bytes,
        &mut ckpt_buf[CKPT_HDR_LEN..CKPT_HDR_LEN + index_len],
    );
    ckpt_buf[CKPT_HDR_LEN + index_len..total_len].copy_from_slice(&tag);

    program_checkpoint(flash, slot, &ckpt_buf[..total_len])?;
    st.d_ckpt = sha256(&ckpt_buf[..total_len]);
    st.active_ckpt_slot = slot;

    // 2. THEN advance hardware counter to e
    if ctr.kind() != CounterKind::None {
        let val = ctr.increment().map_err(|_| Error::CounterExhausted)?;
        assert_eq!(val, e, "Counter drift detected during seal_epoch");
    }

    // 3. open epoch e+1
    st.epoch = e + 1;
    s.roll_epoch(st.epoch);
    st.chain = Chain::anchor(st.epoch, &st.d_ckpt);
    st.records_in_epoch = 0;
    Ok(())
}

/// Helper: Load best checkpoint from flash
#[allow(clippy::type_complexity)]
fn load_best_checkpoint<F: Flash>(
    flash: &mut F,
    s: &mut impl Sealer,
    out_buf: &mut [u8],
) -> Result<Option<(CheckpointHeader, [u8; 32], u8, usize)>, Error> {
    let mut best: Option<(CheckpointHeader, [u8; 32], u8, usize)> = None;
    let mut any_non_empty = false;

    for slot in 0..(CKPT_SLOTS as u8) {
        let blocks_per_slot = crate::config::MAX_CKPT_LEN / flash.block_size() as u32;
        let block_addr = (crate::config::CKPT_BASE_BLOCK + slot as u32 * blocks_per_slot) * flash.block_size() as u32;
        let mut hdr_bytes = [0u8; CKPT_HDR_LEN];

        if flash.read(block_addr, &mut hdr_bytes).is_err() {
            continue;
        }

        if hdr_bytes[0] != crate::config::ERASED_BYTE {
            any_non_empty = true;
        }

        if let Ok(hdr) = CheckpointHeader::decode(&hdr_bytes) {
            let ct_len = hdr.ct_len as usize;
            let total_len = CKPT_HDR_LEN + ct_len;
            if total_len > out_buf.len() || ct_len < 16 {
                continue;
            }
            if flash
                .read(
                    block_addr + CKPT_HDR_LEN as u32,
                    &mut out_buf[CKPT_HDR_LEN..total_len],
                )
                .is_err()
            {
                continue;
            }

            let mut tag = [0u8; 16];
            tag.copy_from_slice(&out_buf[total_len - 16..total_len]);
            let plain_len = ct_len - 16;

            if s.open_checkpoint(
                hdr.epoch,
                slot,
                &hdr_bytes,
                &mut out_buf[CKPT_HDR_LEN..CKPT_HDR_LEN + plain_len],
                &tag,
            )
            .is_ok()
            {
                let is_better = match &best {
                    Some((best_hdr, _, _, _)) => hdr.epoch > best_hdr.epoch,
                    None => true,
                };
                if is_better {
                    out_buf[..CKPT_HDR_LEN].copy_from_slice(&hdr_bytes);
                    let d_ckpt = sha256(&out_buf[..total_len]);
                    best = Some((hdr, d_ckpt, slot, plain_len));
                }
            }
        }
    }

    if best.is_none() && any_non_empty {
        return Err(Error::Tampered);
    }

    Ok(best)
}

/// Full mount (§3.4.1: the tip check is O(1); the replay is O(Θ) — Thm 4.3).
pub fn mount<F: Flash, C: MonotonicCounter>(
    flash: &mut F,
    ctr: &mut C,
    s: &mut impl Sealer,
    out_buf: &mut [u8],
) -> Result<(EngineState, usize), MountError> {
    if flash.capacity() > (1 << crate::config::OFF_BITS) {
        return Err(MountError::FormatError);
    }

    // (a) load newest valid checkpoint: try both slots, verify AEAD, take max epoch.
    let ckpt_opt = load_best_checkpoint(flash, s, out_buf)?;

    let (ckpt, d_ckpt, slot, plain_len) = match ckpt_opt {
        Some(c) => c,
        None => return Err(MountError::FormatError), // Or handle genesis
    };

    // (b) O(1) FRESHNESS-TIP CHECK — Lemma 6.4 boot rule:
    let mc = if ctr.kind() == CounterKind::None {
        u64::MAX
    } else {
        ctr.read().map_err(|_| MountError::Io)?
    };
    let m = ckpt.epoch;

    match ctr.kind() {
        CounterKind::Hardware | CounterKind::BestEffort => {
            if m < mc {
                return Err(MountError::Rollback); // m < MC* => stale epoch
            }
            if m > mc + 1 {
                return Err(MountError::Tampered); // unreachable without forgery
            }
            // accepted: m ∈ {MC*, MC*+1}. If m == MC*+1 we crashed inside the
            // seal window: RE-RUN step 2 now.
            if m == mc + 1 {
                ctr.increment().map_err(|_| MountError::Io)?;
            }
        }
        CounterKind::None => { /* G3 unavailable: record degraded mode, no check */ }
    }

    // (c) re-anchor chain from the checkpoint
    let st = EngineState {
        epoch: ckpt.epoch + 1,
        next_seq: ckpt.seq,
        acked_seq: ckpt.seq.saturating_sub(1),
        d_ckpt,
        chain: Chain::anchor(ckpt.epoch + 1, &d_ckpt),
        records_in_epoch: 0,
        security_mode: match ctr.kind() {
            CounterKind::Hardware => SecurityMode::Full,
            CounterKind::BestEffort => SecurityMode::BestEffortRollback,
            CounterKind::None => SecurityMode::NoRollbackProtection,
        },
        active_ckpt_slot: slot,
    };

    Ok((st, plain_len))
}
