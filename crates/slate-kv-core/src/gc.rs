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

pub fn compact_one<
    'a,
    F: slate_kv_hal::Flash,
    C: slate_kv_hal::MonotonicCounter,
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
    let mut rec_bytes = [0u8; crate::config::REC_OVERHEAD
        + crate::config::MAX_KEY_LEN
        + crate::config::MAX_VAL_LEN];
    let mut scratch = [0u8; crate::config::MAX_KEY_LEN + crate::config::MAX_VAL_LEN];

    while off < seg_base + crate::config::SEG_BYTES as u32 {
        if st.flash.read(off, &mut buf).is_err() {
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
                if st.flash.read(off, &mut hdr_bytes).is_err() {
                    break;
                }
                if let Ok(hdr) = crate::record::RecordHeader::decode(&hdr_bytes) {
                    let total_len =
                        crate::config::REC_OVERHEAD + hdr.klen as usize + hdr.vlen as usize;
                    if total_len <= rec_bytes.len()
                        && st.flash.read(off, &mut rec_bytes[..total_len]).is_ok()
                    {
                        if st
                            .sealer
                            .open_record(
                                &hdr_bytes,
                                &rec_bytes[crate::config::REC_HDR_LEN..total_len],
                                &mut scratch,
                            )
                            .is_ok()
                        {
                            let key = &scratch[..hdr.klen as usize];
                            let val =
                                &scratch[hdr.klen as usize..hdr.klen as usize + hdr.vlen as usize];

                            if hdr.op == crate::config::OP_PUT {
                                let mut is_live = false;
                                let mut cbuf = crate::index::CandidateBuf::new();
                                st.index.candidates(key, &mut cbuf);
                                if cbuf.as_slice().contains(&off) {
                                    is_live = true;
                                }
                                if is_live {
                                    let new_off = st.append_cold(key, val, 0)?;
                                    st.index_update_offset(key, new_off)?;
                                }
                            } else if hdr.op == crate::config::OP_DEL && hdr.seq > watermark {
                                st.append_cold_tombstone(key, 0)?;
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

    if st.cold_batch_full() {
        st.commit()?;
    }

    st.commit()?;

    for b in 0..(crate::config::SEG_BLOCKS_DATA + crate::config::SEG_BLOCKS_PARITY) {
        st.flash
            .erase(seg_base + (b as u32) * st.flash.block_size() as u32)
            .map_err(|_| Error::Io)?;
    }
    st.segs.entries[victim as usize].reset_to_free();
    Ok(())
}
