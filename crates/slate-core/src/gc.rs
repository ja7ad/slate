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
    F: slate_hal::Flash,
    C: slate_hal::MonotonicCounter,
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

    // Mock record scan and processing
    let rec_op = crate::config::OP_PUT; // Mock
    let rec_seq = 0; // Mock
    let is_live = true; // Mock

    if rec_op == crate::config::OP_PUT && is_live {
        let new_off = st.append_cold(b"mock_key", b"mock_val", 0)?;
        st.index_update_offset(b"mock_key", new_off);
    } else if rec_op == crate::config::OP_DEL && rec_seq > watermark {
        st.append_cold_tombstone(b"mock_key", 0)?;
    }

    if st.cold_batch_full() {
        st.commit()?;
    }

    st.commit()?;

    let block_size = 4096 * 12; // 12 blocks per segment
    st.flash.erase(victim * block_size).map_err(|_| Error::Io)?;
    st.segs.entries[victim as usize].reset_to_free();
    Ok(())
}
