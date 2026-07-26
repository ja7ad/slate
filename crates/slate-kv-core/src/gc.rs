//! gc
#![allow(missing_docs)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::needless_range_loop)]

use crate::error::Error;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SegState {
    OpenHot,
    OpenCold,
    Sealed,
    Free,
}

#[derive(Clone, Copy, Debug)]
pub struct SegEntry {
    pub id: u32,
    pub live_bytes: u16,
    pub minseq: u64,
    pub state: SegState,
    pub seg_seq: u64,
}

impl SegEntry {
    pub fn new(id: u32) -> Self {
        Self {
            id,
            live_bytes: 0,
            minseq: u64::MAX,
            state: SegState::Free,
            seg_seq: 0,
        }
    }

    pub fn reset_to_free(&mut self) {
        self.live_bytes = 0;
        self.minseq = u64::MAX;
        self.state = SegState::Free;
        self.seg_seq = 0;
    }
}

pub const MAX_SEGMENTS: usize = 128;

pub struct SegTable {
    pub entries: [SegEntry; MAX_SEGMENTS],
    pub num_segments: u32,
}

impl SegTable {
    pub fn new(num_segments: u32) -> Self {
        let mut entries = [SegEntry::new(0); MAX_SEGMENTS];
        for i in 0..core::cmp::min(num_segments as usize, MAX_SEGMENTS) {
            entries[i] = SegEntry::new(i as u32);
        }
        Self {
            entries,
            num_segments,
        }
    }

    pub fn pick_victim(&self, ckpt_seg_seq: u64) -> Option<u32> {
        self.entries[..self.num_segments as usize]
            .iter()
            .filter(|s| s.state == SegState::Sealed && s.seg_seq < ckpt_seg_seq)
            .min_by_key(|s| s.live_bytes)
            .map(|s| s.id)
    }

    pub fn update_live_bytes(&mut self, id: u32, delta: i32) {
        if id < self.num_segments {
            let entry = &mut self.entries[id as usize];
            let current = entry.live_bytes as i32;
            let new_val = core::cmp::max(0, current + delta);
            entry.live_bytes = new_val as u16;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn reappend_cold<F: slate_kv_hal::AsyncFlash, S: crate::log::Sealer>(
    log_cold: &mut crate::log::Log<'_, F>,
    sealer: &mut S,
    engine: &mut crate::epoch::EngineState,
    sched: &mut crate::sched::Scheduler,
    op: u8,
    key: &[u8],
    val: &[u8],
) -> Result<(u32, bool), crate::error::Error> {
    let seq = engine.next_seq;
    let epoch = engine.epoch;
    let (_seq_ret, offset) =
        log_cold.append(seq, epoch, op, key, val, sealer, &mut engine.chain)?;
    engine.next_seq += 1;
    engine.records_in_epoch += 1;
    let need_commit = sched.on_append(0);
    Ok((offset, need_commit))
}

#[allow(clippy::too_many_arguments)]
async fn reindex_update_offset<F: slate_kv_hal::AsyncFlash, S: crate::log::Sealer>(
    index: &mut crate::index::Index<'_>,
    rng: &mut crate::index::XorShift64,
    flash: &mut F,
    sealer: &mut S,
    log_hot: &crate::log::Log<'_, F>,
    log_cold: &crate::log::Log<'_, F>,
    cand_rec: &mut [u8],
    cand_scratch: &mut [u8],
    key: &[u8],
    new_off: u32,
) -> Result<(), crate::error::Error> {
    let hot_base = log_hot.head.write_offset;
    let hot_data = log_hot.batch.data();
    let cold_base = log_cold.head.write_offset;
    let cold_data = log_cold.batch.data();
    let mut cbuf = crate::index::CandidateBuf::new();
    index.candidates(key, &mut cbuf);
    let mut matching_off = None;
    for &cand_off in cbuf.as_slice() {
        if crate::slate::cand_matches_key_async(
            flash,
            sealer,
            hot_base,
            hot_data,
            cold_base,
            cold_data,
            cand_rec,
            cand_scratch,
            cand_off,
            key,
        )
        .await
        {
            matching_off = Some(cand_off);
            break;
        }
    }
    index.upsert(key, new_off, rng, |cand_off| Some(cand_off) == matching_off)
}

pub async fn compact_one_async<
    'a,
    F: slate_kv_hal::AsyncFlash,
    C: slate_kv_hal::AsyncMonotonicCounter,
    S: crate::log::Sealer,
>(
    st: &mut crate::slate::Slate<'a, F, C, S>,
) -> Result<(), Error> {
    let ckpt_seg_seq = st.ckpt_seg_seq;
    let victim = match st.segs.pick_victim(ckpt_seg_seq) {
        Some(v) => v,
        None => return Ok(()),
    };

    assert!(
        st.segs.entries[victim as usize].seg_seq < ckpt_seg_seq,
        "Victim is newer than latest checkpoint"
    );

    // We stub the scan of the segment for now
    let watermark = st.segs.entries[..st.segs.num_segments as usize]
        .iter()
        .filter(|s| s.id != victim && s.state != SegState::Free)
        .map(|s| s.minseq)
        .min()
        .unwrap_or(u64::MAX);

    // Real record scan
    let seg_base = victim * crate::config::SEG_BYTES as u32;
    let page_size = st.flash.page_size() as u32;
    let mut off = seg_base + page_size; // Skip segment header

    let mut buf = [0u8; 1];
    let mut since_yield = 0u16;

    while off < seg_base + crate::config::SEG_BYTES as u32 {
        if since_yield >= crate::config::GC_YIELD_EVERY_RECORDS {
            crate::task::yield_now().await;
            since_yield = 0;
        }
        since_yield += 1;
        if st.flash.read(off, &mut buf).await.is_err() {
            break;
        }

        match buf[0] {
            crate::config::ERASED_BYTE => {
                let rem = off % page_size;
                if rem != 0 {
                    off += page_size - rem;
                } else {
                    break;
                }
            }
            crate::config::MAGIC_CM => {
                off += page_size * 2;
            }
            crate::config::MAGIC_XOR => {
                off += page_size;
            }
            crate::config::MAGIC_REC => {
                let mut hdr_bytes = [0u8; crate::config::REC_HDR_LEN];
                if st.flash.read(off, &mut hdr_bytes).await.is_err() {
                    break;
                }
                if let Ok(hdr) = crate::record::RecordHeader::decode(&hdr_bytes) {
                    let total_len =
                        crate::config::REC_OVERHEAD + hdr.klen as usize + hdr.vlen as usize;
                    if total_len <= st.scratch_buf.gc_rec_bytes.len()
                        && st
                            .flash
                            .read(off, &mut st.scratch_buf.gc_rec_bytes[..total_len])
                            .await
                            .is_ok()
                    {
                        if st
                            .sealer
                            .open_record(
                                &hdr_bytes,
                                &st.scratch_buf.gc_rec_bytes[crate::config::REC_HDR_LEN..total_len],
                                &mut st.scratch_buf.gc_scratch,
                            )
                            .is_ok()
                        {
                            let key_len = hdr.klen as usize;
                            let val_len = hdr.vlen as usize;
                            let key = &st.scratch_buf.gc_scratch[..key_len];
                            let val = &st.scratch_buf.gc_scratch[key_len..key_len + val_len];

                            if hdr.op == crate::config::OP_PUT {
                                let mut is_live = false;
                                let mut cbuf = crate::index::CandidateBuf::new();
                                st.index.candidates(key, &mut cbuf);
                                if cbuf.as_slice().contains(&off) {
                                    is_live = true;
                                }
                                if is_live {
                                    let (new_off, need_commit) = reappend_cold(
                                        &mut st.log_cold,
                                        &mut st.sealer,
                                        &mut st.engine,
                                        &mut st.sched,
                                        crate::config::OP_PUT,
                                        key,
                                        val,
                                    )?;
                                    if need_commit {
                                        st.commit_async().await?;
                                    }
                                    let key = &st.scratch_buf.gc_scratch[..key_len];
                                    reindex_update_offset(
                                        &mut st.index,
                                        &mut st.rng,
                                        &mut st.flash,
                                        &mut st.sealer,
                                        &st.log_hot,
                                        &st.log_cold,
                                        &mut st.scratch_buf.cand_rec_bytes,
                                        &mut st.scratch_buf.cand_scratch,
                                        key,
                                        new_off,
                                    )
                                    .await?;
                                }
                            } else if hdr.op == crate::config::OP_DEL && hdr.seq > watermark {
                                let (_, need_commit) = reappend_cold(
                                    &mut st.log_cold,
                                    &mut st.sealer,
                                    &mut st.engine,
                                    &mut st.sched,
                                    crate::config::OP_DEL,
                                    key,
                                    &[],
                                )?;
                                if need_commit {
                                    st.commit_async().await?;
                                }
                            }
                        }
                    }
                    off += total_len as u32;
                } else {
                    break;
                }
            }
            _ => {
                break;
            }
        }
    }

    if false {
        st.commit_async().await?;
    }

    st.commit_async().await?;

    for b in 0..(crate::config::SEG_BLOCKS_DATA + crate::config::SEG_BLOCKS_PARITY) {
        st.flash
            .erase(seg_base + (b as u32) * st.flash.block_size() as u32)
            .await
            .map_err(|_| Error::Io)?;
        crate::task::yield_now().await;
    }
    st.segs.entries[victim as usize].reset_to_free();
    Ok(())
}
