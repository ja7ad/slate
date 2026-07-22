//! epoch
#![allow(missing_docs)]

use crate::chain::Chain;
use crate::checkpoint::{CKPT_HDR_LEN, CheckpointHeader};
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

/// Helper to serialize the snapshot (in a real system this would serialize Index, etc.)
fn encode_checkpoint(
    _st: &EngineState,
    s: &mut impl Sealer,
    e: u64,
    slot: u8,
    out: &mut [u8],
) -> Result<usize, Error> {
    // For now we just stub the snapshot with dummy bytes since doc 005 uses it conceptually
    let snapshot = b"dummy_snapshot";

    let hdr = CheckpointHeader {
        magic: crate::config::MAGIC_CKPT,
        format_version: 1,
        epoch: e,
        seq: 0,     // from log
        seg_seq: 0, // from log head
        write_offset: 0,
        n_keys: 0, // from index
        ct_len: snapshot.len() as u32 + 16,
    };

    let total_len = CKPT_HDR_LEN + hdr.ct_len as usize;
    if out.len() < total_len {
        return Err(Error::FormatError);
    }

    let mut hdr_bytes = [0u8; CKPT_HDR_LEN];
    hdr.encode(&mut hdr_bytes);
    out[..CKPT_HDR_LEN].copy_from_slice(&hdr_bytes);

    s.seal_checkpoint(
        e,
        slot,
        &hdr_bytes,
        snapshot,
        &mut out[CKPT_HDR_LEN..total_len],
    );
    Ok(total_len)
}

fn program_checkpoint<F: Flash>(flash: &mut F, slot: u8, bytes: &[u8]) -> Result<(), Error> {
    // In doc 002, checkpoint area is after superblock. We assume it's at block 2 and 3 for example.
    let block_addr = (2 + slot as u32) * flash.block_size() as u32;
    flash.erase(block_addr).map_err(|_| Error::Io)?;
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
pub fn seal_epoch<F: Flash, C: MonotonicCounter>(
    st: &mut EngineState,
    flash: &mut F,
    ctr: &mut C,
    s: &mut impl Sealer, // Sealer provides roll_epoch
) -> Result<(), Error> {
    let e = st.epoch;

    // 1. write + flush checkpoint carrying counter field e
    let slot = st.next_ckpt_slot();
    let mut ckpt_buf = [0u8; 1024]; // Buffer for checkpoint (stubbed size)
    let len = encode_checkpoint(st, s, e, slot, &mut ckpt_buf)?;
    program_checkpoint(flash, slot, &ckpt_buf[..len])?;
    st.d_ckpt = sha256(&ckpt_buf[..len]);
    st.active_ckpt_slot = slot;

    // 2. THEN advance hardware counter to e
    if ctr.kind() != CounterKind::None {
        ctr.increment().map_err(|_| Error::Io)?;
    }

    // 3. open epoch e+1
    st.epoch = e + 1;
    s.roll_epoch(st.epoch);
    st.chain = Chain::anchor(st.epoch, &st.d_ckpt);
    st.records_in_epoch = 0;
    Ok(())
}

fn load_best_checkpoint<F: Flash>(
    _flash: &mut F,
    _s: &mut impl Sealer,
) -> Result<Option<(CheckpointHeader, [u8; 32], u8)>, Error> {
    // Stub: returns a dummy genesis checkpoint for now
    Ok(Some((
        CheckpointHeader {
            magic: crate::config::MAGIC_CKPT,
            format_version: 1,
            epoch: 0,
            seq: 0,
            seg_seq: 0,
            write_offset: 0,
            n_keys: 0,
            ct_len: 16,
        },
        [0u8; 32], // d_ckpt
        0,         // slot
    )))
}

/// Full mount (§3.4.1: the tip check is O(1); the replay is O(Θ) — Thm 4.3).
pub fn mount<F: Flash, C: MonotonicCounter>(
    flash: &mut F,
    ctr: &mut C,
    s: &mut impl Sealer,
) -> Result<EngineState, MountError> {
    if flash.capacity() > (1 << crate::config::OFF_BITS) {
        return Err(MountError::FormatError);
    }

    // (a) load newest valid checkpoint: try both slots, verify AEAD, take max epoch.
    let ckpt_opt = load_best_checkpoint(flash, s)?;

    let (ckpt, d_ckpt, slot) = match ckpt_opt {
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
            if m + 1 < mc + 1 && m < mc {
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

    // (c) re-anchor chain from the checkpoint and O(Θ) replay of the tail
    let st = EngineState {
        epoch: ckpt.epoch,
        next_seq: ckpt.seq,
        acked_seq: ckpt.seq.saturating_sub(1),
        d_ckpt,
        chain: Chain::anchor(ckpt.epoch, &d_ckpt),
        records_in_epoch: 0,
        security_mode: match ctr.kind() {
            CounterKind::Hardware => SecurityMode::Full,
            CounterKind::BestEffort => SecurityMode::BestEffortRollback,
            CounterKind::None => SecurityMode::NoRollbackProtection,
        },
        active_ckpt_slot: slot,
    };

    // recover_tail(flash, &mut st)?; // Stubbed: O(Θ) replay of the tail

    Ok(st)
}
