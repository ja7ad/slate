//! epoch
#![allow(missing_docs)]

use crate::chain::Chain;
use crate::checkpoint::{CheckpointHeader, CKPT_HDR_LEN};
use crate::config::CKPT_SLOTS;
use crate::error::Error;
use crate::log::Sealer;
use sha2::{Digest, Sha256};
use slate_kv_hal::{AsyncFlash, AsyncMonotonicCounter, CounterKind};
#[cfg(feature = "blocking")]
use slate_kv_hal::{Flash, MonotonicCounter};

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

/// Programs a checkpoint into `slot`, returning `(bytes_programmed, erases)` so
/// the caller can attribute the traffic to the checkpoint bucket of the
/// write-amplification accounting.
async fn program_checkpoint<F: AsyncFlash>(
    flash: &mut F,
    slot: u8,
    bytes: &[u8],
    page_buf: &mut [u8],
) -> Result<(u64, u64), Error> {
    // Checkpoint area sits above the superblock; slot addressing is shared with
    // the reader and with `data_base_offset` so the three can never disagree.
    let block_addr = crate::config::ckpt_slot_addr(slot, flash.block_size());

    // Erase enough blocks for the serialized checkpoint
    let num_blocks = bytes.len().div_ceil(flash.block_size());
    let mut erases = 0u64;
    for i in 0..num_blocks {
        flash
            .erase(block_addr + (i * flash.block_size()) as u32)
            .await
            .map_err(|_| Error::Io)?;
        erases += 1;
        crate::task::yield_now().await;
    }
    // Pad bytes to page size
    let page_size = flash.page_size();
    // A device page must fit the fixed-size staging buffer; a larger page would
    // otherwise silently write past it.
    if page_size > crate::config::MAX_PAGE_SIZE || page_buf.len() < page_size {
        return Err(Error::FormatError);
    }
    let num_pages = bytes.len().div_ceil(page_size);

    for i in 0..num_pages {
        let start = i * page_size;
        let end = core::cmp::min(start + page_size, bytes.len());
        page_buf[..end - start].copy_from_slice(&bytes[start..end]);
        if end - start < page_size {
            page_buf[end - start..page_size].fill(0xFF);
        }
        flash
            .program(block_addr + start as u32, &page_buf[..page_size])
            .await
            .map_err(|_| Error::Io)?;
        crate::task::yield_now().await;
    }
    Ok(((num_pages * page_size) as u64, erases))
}

/// Flash traffic produced by one epoch seal, for write-amplification
/// accounting. Checkpoint pages are engine overhead: the application never
/// asked for them, but they consume endurance and must appear in the numerator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CkptCost {
    /// Checkpoint pages programmed.
    pub bytes: u64,
    /// Blocks erased to make room for them.
    pub erases: u64,
}

/// EPOCH SEAL — the write-ahead protocol.
#[allow(clippy::too_many_arguments)]
pub async fn seal_epoch_async<F: AsyncFlash, C: AsyncMonotonicCounter>(
    st: &mut EngineState,
    flash: &mut F,
    ctr: &mut C,
    s: &mut impl Sealer,
    seg_seq: u64,
    write_offset: u32,
    n_keys: u16,
    ckpt_buf: &mut [u8],
    index_len: usize,
    page_buf: &mut [u8],
) -> Result<CkptCost, Error> {
    let e = st.epoch;

    // The next epoch must remain representable in the record nonce's 32-bit
    // epoch discriminator; past that, two epochs would share a record key
    // namespace and AEAD nonce uniqueness would no longer be guaranteed. At Θ
    // records per epoch this ceiling is ~7e13 writes away, so reaching it means
    // something is wrong, but the engine must refuse rather than wrap.
    if e >= crate::record::MAX_REC_EPOCH {
        return Err(Error::CounterExhausted);
    }

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

    let (ckpt_bytes, ckpt_erases) =
        program_checkpoint(flash, slot, &ckpt_buf[..total_len], page_buf).await?;
    st.d_ckpt = sha256(&ckpt_buf[..total_len]);
    st.active_ckpt_slot = slot;

    // 2. THEN advance hardware counter to e
    if ctr.kind() != CounterKind::None {
        let val = ctr.increment().await.map_err(|_| Error::CounterExhausted)?;
        assert_eq!(val, e, "Counter drift detected during seal_epoch");
    }

    // 3. open epoch e+1
    st.epoch = e + 1;
    s.roll_epoch(st.epoch);
    st.chain = Chain::anchor(st.epoch, &st.d_ckpt);
    st.records_in_epoch = 0;
    Ok(CkptCost {
        bytes: ckpt_bytes,
        erases: ckpt_erases,
    })
}

#[cfg(feature = "blocking")]
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
    page_buf: &mut [u8],
) -> Result<CkptCost, Error> {
    crate::task::block_on(seal_epoch_async(
        st,
        &mut slate_kv_hal::BlockingFlash(flash),
        &mut slate_kv_hal::BlockingCounter(ctr),
        s,
        seg_seq,
        write_offset,
        n_keys,
        ckpt_buf,
        index_len,
        page_buf,
    ))
}

#[cfg(not(feature = "blocking"))]
pub use seal_epoch_async as seal_epoch;

/// Helper: Load best checkpoint from flash
#[allow(clippy::type_complexity)]
async fn load_best_checkpoint_async<F: AsyncFlash>(
    flash: &mut F,
    s: &mut impl Sealer,
    out_buf: &mut [u8],
    slots_verified: &mut u8,
) -> Result<Option<(CheckpointHeader, [u8; 32], u8, usize)>, Error> {
    let mut best: Option<(CheckpointHeader, [u8; 32], u8, usize)> = None;
    let mut any_non_empty = false;
    *slots_verified = 0;

    for slot in 0..(CKPT_SLOTS as u8) {
        let block_addr = crate::config::ckpt_slot_addr(slot, flash.block_size());
        let mut hdr_bytes = [0u8; CKPT_HDR_LEN];

        if flash.read(block_addr, &mut hdr_bytes).await.is_err() {
            crate::task::yield_now().await;
            continue;
        }

        if hdr_bytes[0] != crate::config::ERASED_BYTE {
            any_non_empty = true;
        }

        if let Ok(hdr) = CheckpointHeader::decode(&hdr_bytes) {
            let ct_len = hdr.ct_len as usize;
            let total_len = CKPT_HDR_LEN + ct_len;
            if total_len > out_buf.len() || ct_len < 16 {
                crate::task::yield_now().await;
                continue;
            }
            if flash
                .read(
                    block_addr + CKPT_HDR_LEN as u32,
                    &mut out_buf[CKPT_HDR_LEN..total_len],
                )
                .await
                .is_err()
            {
                crate::task::yield_now().await;
                continue;
            }

            let mut tag = [0u8; 16];
            tag.copy_from_slice(&out_buf[total_len - 16..total_len]);
            let plain_len = ct_len - 16;

            // D_ckpt must be taken over the *sealed* bytes exactly as they sit
            // on flash, because that is what `seal_epoch` hashed when it set
            // `st.d_ckpt`. Hashing after `open_checkpoint` decrypts in place
            // would digest the plaintext instead, yielding a different anchor
            // for `Chain::anchor` — and then every commit marker written by the
            // engine that sealed this checkpoint fails its χ comparison during
            // replay. The tail is silently discarded: the database comes back
            // up, reports no error, and is missing every record written since
            // the last checkpoint.
            out_buf[..CKPT_HDR_LEN].copy_from_slice(&hdr_bytes);
            let d_ckpt = sha256(&out_buf[..total_len]);

            if s.open_checkpoint(
                hdr.epoch,
                slot,
                &hdr_bytes,
                &mut out_buf[CKPT_HDR_LEN..CKPT_HDR_LEN + plain_len],
                &tag,
            )
            .is_ok()
            {
                *slots_verified += 1;
                let is_better = match &best {
                    Some((best_hdr, _, _, _)) => hdr.epoch > best_hdr.epoch,
                    None => true,
                };
                if is_better {
                    best = Some((hdr, d_ckpt, slot, plain_len));
                }
            }
        }
        crate::task::yield_now().await;
    }

    if best.is_none() && any_non_empty {
        return Err(Error::Tampered);
    }

    Ok(best)
}

/// Full mount (§3.4.1: the tip check is O(1); the replay is O(Θ) — Thm 4.3).
/// What [`mount`] recovered from the newest valid checkpoint.
pub struct MountInfo {
    /// Engine state re-anchored to the checkpoint.
    pub state: EngineState,
    /// Length of the serialized index in `out_buf`, after the header.
    pub plain_len: usize,
    /// Log head at the instant the checkpoint was sealed.
    ///
    /// Every record below this offset is already reflected in the checkpointed
    /// index, so tail replay must start *here*, not at the base of the log.
    /// Replaying from the base is not merely slow — it makes mount cost O(log
    /// length) instead of the O(Θ) the design promises — it is also wrong: those
    /// older records carry commit markers from earlier epochs, which no longer
    /// validate against the chain re-anchored to the current epoch.
    pub ckpt_write_offset: u32,
    /// Segment sequence at checkpoint time; the GC reclaim watermark.
    pub ckpt_seg_seq: u64,
    /// How many of the `CKPT_SLOTS` checkpoint slots held a checkpoint that read
    /// back and passed its AEAD check.
    ///
    /// Mount reads and verifies *every* populated slot before picking the newest,
    /// so this is what separates the fixed part of mount's read cost from the
    /// part that scales with the replay tail. It is bounded by `CKPT_SLOTS`, so it
    /// does not make mount grow with volume — but a reader comparing two mount
    /// measurements needs to know which of them paid for one slot and which for
    /// two.
    pub ckpt_slots_verified: u8,
}

pub async fn mount_async<F: AsyncFlash, C: AsyncMonotonicCounter>(
    flash: &mut F,
    ctr: &mut C,
    s: &mut impl Sealer,
    out_buf: &mut [u8],
) -> Result<MountInfo, MountError> {
    if flash.capacity() > (1 << crate::config::OFF_BITS) {
        return Err(MountError::FormatError);
    }

    // (a) load newest valid checkpoint: try both slots, verify AEAD, take max epoch.
    // Every populated slot is read in full and AEAD-verified, so the *number* of
    // populated slots is a real (bounded) term in mount's read cost, not a
    // detail: a volume that has sealed at least CKPT_SLOTS times pays for all of
    // them on every mount.
    let mut ckpt_slots_verified = 0u8;
    let ckpt_opt = load_best_checkpoint_async(flash, s, out_buf, &mut ckpt_slots_verified).await?;

    let (ckpt, d_ckpt, slot, plain_len) = match ckpt_opt {
        Some(c) => c,
        None => return Err(MountError::FormatError), // Or handle genesis
    };

    // (b) O(1) FRESHNESS-TIP CHECK — Lemma 6.4 boot rule:
    let mc = if ctr.kind() == CounterKind::None {
        u64::MAX
    } else {
        ctr.read().await.map_err(|_| MountError::Io)?
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
                ctr.increment().await.map_err(|_| MountError::Io)?;
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

    Ok(MountInfo {
        state: st,
        plain_len,
        ckpt_write_offset: ckpt.write_offset,
        ckpt_seg_seq: ckpt.seg_seq,
        ckpt_slots_verified,
    })
}

#[cfg(feature = "blocking")]
pub fn mount<F: Flash, C: MonotonicCounter>(
    flash: &mut F,
    ctr: &mut C,
    s: &mut impl Sealer,
    out_buf: &mut [u8],
) -> Result<MountInfo, MountError> {
    crate::task::block_on(mount_async(
        &mut slate_kv_hal::BlockingFlash(flash),
        &mut slate_kv_hal::BlockingCounter(ctr),
        s,
        out_buf,
    ))
}

#[cfg(not(feature = "blocking"))]
pub use mount_async as mount;
