//! Regression tests for the flash-region sizing contract that every port must
//! satisfy before it can store a single record.
//!
//! Background: the ESP32 demo binaries mapped a 512 KiB region
//! (`4096 * 128` = 524 288 bytes) while the on-flash format reserves
//! `data_base_offset(4096)` = 540 672 bytes for the superblock and the two
//! checkpoint slots. The append head therefore started *above* `capacity()`,
//! so `mount` succeeded and every subsequent `program` returned an
//! out-of-bounds error — the volume accepted nothing, with no diagnostic
//! pointing at the region size. These tests fail loudly if the format grows
//! past a region size a port still believes is adequate.

use slate_kv_core::config::{
    ckpt_blocks_per_slot, ckpt_slot_addr, data_base_offset, CKPT_BASE_BLOCK, CKPT_SLOTS,
    MAX_CKPT_LEN, SEG_BYTES,
};

/// Block size used by the ESP32/file backends.
const BS: usize = 4096;

/// The region the ESP32 demos map today (2 MiB), and the one they used to map.
const ESP32_REGION_NOW: u32 = 0x200000;
const ESP32_REGION_OLD: u32 = 4096 * 128;

/// Minimum bytes a region needs to hold the reserved layout plus `n` segments.
fn min_region_for(n_segments: u32, block_size: usize) -> u64 {
    data_base_offset(block_size) as u64 + n_segments as u64 * SEG_BYTES as u64
}

#[test]
fn reserved_region_precedes_first_log_byte() {
    let data_base = data_base_offset(BS);
    let last_slot_end = ckpt_slot_addr(CKPT_SLOTS as u8 - 1, BS) + MAX_CKPT_LEN;
    assert!(
        data_base >= last_slot_end,
        "data_base_offset ({data_base}) must clear the last checkpoint slot \
         (ends at {last_slot_end}); the append log would program live \
         checkpoint pages"
    );
}

#[test]
fn checkpoint_slots_do_not_overlap() {
    let span = ckpt_blocks_per_slot(BS) as usize * BS;
    assert!(
        span >= MAX_CKPT_LEN as usize,
        "one slot spans {span} bytes but MAX_CKPT_LEN is {MAX_CKPT_LEN}"
    );
    for slot in 1..CKPT_SLOTS as u8 {
        let prev_end = ckpt_slot_addr(slot - 1, BS) + MAX_CKPT_LEN;
        assert!(
            ckpt_slot_addr(slot, BS) >= prev_end,
            "slot {slot} starts inside slot {}",
            slot - 1
        );
    }
}

/// The bug that produced the field report: the old 512 KiB region cannot even
/// reach the first writable log byte.
#[test]
fn old_esp32_region_was_too_small() {
    let data_base = data_base_offset(BS);
    assert!(
        (ESP32_REGION_OLD as u64) < data_base as u64,
        "this test encodes the historical bug: {ESP32_REGION_OLD} was expected \
         to be smaller than data_base {data_base}"
    );
}

/// The region the demos map now must hold the reserved layout plus a useful
/// number of segments. If the format grows, this fails here rather than on a
/// board.
#[test]
fn current_esp32_region_fits_reserved_layout_and_segments() {
    let data_base = data_base_offset(BS);
    assert!(
        ESP32_REGION_NOW as u64 > data_base as u64,
        "region {ESP32_REGION_NOW} does not even reach the first log byte \
         ({data_base})"
    );

    // Require headroom for at least four segments: the compactor needs a victim
    // plus somewhere to re-append its live records.
    let need = min_region_for(4, BS);
    assert!(
        ESP32_REGION_NOW as u64 >= need,
        "region {ESP32_REGION_NOW} < {need} needed for reserved layout + 4 \
         segments (SEG_BYTES={SEG_BYTES})"
    );
}

/// A `SegTable` sized larger than the region can address lets `pick_victim`
/// return a segment whose base lies past `capacity()`, so compaction erases
/// out of bounds. Ports must derive the count from the region length.
#[test]
fn segment_count_must_be_derived_from_region_length() {
    let data_base = data_base_offset(BS);
    let usable = ESP32_REGION_NOW - data_base;
    let fits = usable / SEG_BYTES as u32;

    assert!(fits >= 4, "only {fits} segments fit; expected at least 4");

    // The hardcoded 128 the demos used is not addressable in this region.
    assert!(
        fits < 128,
        "this test assumes the 2 MiB region holds fewer than 128 segments; \
         got {fits} — update the demos if the geometry changed"
    );

    // And the last addressable segment must end inside the region.
    let last_end = data_base as u64 + fits as u64 * SEG_BYTES as u64;
    assert!(
        last_end <= ESP32_REGION_NOW as u64,
        "segment {fits} ends at {last_end}, past the region end \
         {ESP32_REGION_NOW}"
    );
}

/// The layout must be correct for any plausible block size, not just 4 KiB.
#[test]
fn layout_holds_across_block_sizes() {
    for &bs in &[256usize, 512, 1024, 4096, 65536] {
        let data_base = data_base_offset(bs);
        let last_slot_end = ckpt_slot_addr(CKPT_SLOTS as u8 - 1, bs) + MAX_CKPT_LEN;
        assert!(
            data_base >= last_slot_end,
            "block_size {bs}: data_base {data_base} < last slot end \
             {last_slot_end}"
        );
        assert_eq!(
            data_base as usize % bs,
            0,
            "block_size {bs}: data_base {data_base} is not block-aligned, so \
             the first log write would straddle an erase block"
        );
        assert!(
            data_base >= CKPT_BASE_BLOCK * bs as u32,
            "block_size {bs}: data_base below the checkpoint base block"
        );
    }
}
