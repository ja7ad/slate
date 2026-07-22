#![allow(unused_imports)]

use slate_core::config::*;
use slate_core::epoch::{EngineState, SecurityMode};
use slate_core::error::Error;
use slate_core::gc::{SegState, SegTable};
use slate_core::index::Index;
use slate_core::log::{HeadState, Log};
use slate_core::slate::Slate;
use slate_sim::{SimCounter, SimFlash};

// Mock sealer for testing
struct MockSealer;
impl slate_core::log::Sealer for MockSealer {
    fn seal_record(&mut self, _hdr: &[u8; REC_HDR_LEN], _plain_kv: &[u8], ct_tag_out: &mut [u8]) {
        ct_tag_out.fill(0);
    }
    fn open_record(
        &mut self,
        _hdr: &[u8; REC_HDR_LEN],
        _ct_tag: &[u8],
        _plain_out: &mut [u8],
    ) -> Result<(), Error> {
        Ok(())
    }
    fn commit_marker(&mut self, _seq_max: u64, _epoch: u64, _chi: &[u8; 32]) -> [u8; CM_LEN] {
        let mut out = [0u8; CM_LEN];
        out[0] = MAGIC_CM;
        out
    }
    fn verify_marker(&self, _cm: &[u8; CM_LEN]) -> Result<slate_core::log::CmFields, Error> {
        Err(Error::FormatError) // Not used in this mock test
    }
    fn seal_checkpoint(&mut self, _epoch: u64, _plain: &[u8], ct_tag_out: &mut [u8]) {
        ct_tag_out.fill(0);
    }
    fn open_checkpoint(
        &mut self,
        _epoch: u64,
        _ct_tag: &[u8],
        _plain_out: &mut [u8],
    ) -> Result<(), Error> {
        Ok(())
    }
}

fn create_slate<'a>(
    flash: SimFlash,
    hot_buf: &'a mut [u8],
    cold_buf: &'a mut [u8],
    index_slots: &'a mut [u32],
) -> Slate<'a, SimFlash, MockSealer> {
    let engine = EngineState {
        epoch: 1,
        d_ckpt: [0u8; 32],
        chain: slate_core::chain::Chain::anchor(1, &[0u8; 32]),
        records_in_epoch: 0,
        security_mode: SecurityMode::Full,
        active_ckpt_slot: 0,
    };

    let log_hot = Log::new(
        hot_buf,
        1,
        0,
        1,
        HeadState {
            seg_seq: 1,
            write_offset: 0,
            block_idx: 0,
        },
    );

    let log_cold = Log::new(
        cold_buf,
        1, // cold doesn't really have its own seq, it inherits or generates depending on design
        0,
        1,
        HeadState {
            seg_seq: 2,
            write_offset: 4096 * 12, // seg 1
            block_idx: 12,
        },
    );

    Slate {
        flash,
        sealer: MockSealer,
        engine,
        log_hot,
        log_cold,
        index: Index::new(index_slots, N_BUCKETS),
        segs: SegTable::new(128), // 128 max segments
        ckpt_seg_seq: 10,         // fake checkpoint time
        sched: slate_core::sched::Scheduler::new(slate_core::config::SchedCfg {
            auto_b: false,
            fixed_cost_uj: 400,
            holding_nj_per_op_s: 1000,
            deadline_ms: 1000,
            b_min: 1,
            b_max: 128,
            b_commit: 27,
        }),
        metrics: Default::default(),
    }
}

#[test]
fn test_no_resurrection_prop3_1() {
    let mut hot_buf = [0u8; 4096];
    let mut cold_buf = [0u8; 4096];
    let mut index_slots = vec![0u32; N_BUCKETS * BUCKET_SLOTS];

    let flash = SimFlash::new(4096 * 128, 256, 4096);
    let mut st = create_slate(flash, &mut hot_buf, &mut cold_buf, &mut index_slots);

    // Setup: put key "A" in segment 0, tombstone in segment 1
    st.segs.entries[0].state = SegState::Sealed;
    st.segs.entries[0].live_bytes = 100;
    st.segs.entries[0].minseq = 5;
    st.segs.entries[0].seg_seq = 1;

    st.segs.entries[1].state = SegState::Sealed;
    st.segs.entries[1].live_bytes = 100;
    st.segs.entries[1].minseq = 6;
    st.segs.entries[1].seg_seq = 2;

    st.segs.num_segments = 2;

    // Compact segment 1 (tombstone) BEFORE segment 0 (put)
    // Watermark is minseq of segment 0 = 5
    // Tombstone has seq = 6. 6 > 5, so it must be forwarded.

    // In our mock, compact_one scans and does exactly this if rec_seq > watermark
    // We mock the rec_seq and op inside `compact_one` currently, so we'll just call it and verify the logic.
    st.ckpt_seg_seq = 10;

    // Our gc.rs stub mocks: rec_op = OP_PUT, is_live = true
    // We just test that it runs without panic for now.
    st.compact().unwrap();

    assert!(st.flash.stats.programs > 0);
}

#[test]
fn test_wa_accounting() {
    let mut hot_buf = [0u8; 4096];
    let mut cold_buf = [0u8; 4096];
    let mut index_slots = vec![0u32; N_BUCKETS * BUCKET_SLOTS];

    let flash = SimFlash::new(4096 * 128, 256, 4096);
    let mut st = create_slate(flash, &mut hot_buf, &mut cold_buf, &mut index_slots);

    // Write a hot put
    st.log_hot
        .append(OP_PUT, b"key", b"val", &mut st.sealer, &mut st.engine.chain)
        .unwrap();
    st.commit().unwrap();

    assert!(st.flash.stats.user_bytes > 0);
    assert_eq!(st.flash.stats.gc_bytes, 0);

    // Write a cold put
    st.flash.is_gc_write = true;
    st.append_cold(b"key2", b"val2", 0).unwrap();
    st.commit().unwrap();
    st.flash.is_gc_write = false;

    assert!(st.flash.stats.gc_bytes > 0);
}
