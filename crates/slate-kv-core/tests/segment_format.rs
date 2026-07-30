//! Segment header codec and geometry.
//!
//! `SegmentHeader::encode`/`decode` define an on-flash layout, so their field
//! offsets are a compatibility contract: a change here silently orphans every
//! volume written by an older build. They had no test and no caller (parity
//! encoding is not yet wired into a live volume — see the space-reuse gap in
//! the specification), which meant a byte-offset regression would have been
//! invisible until it corrupted a device.
//!
//! These tests pin the wire format explicitly rather than round-tripping
//! through the same code that would carry the bug.

use slate_kv_core::config::MAGIC_SEG;
use slate_kv_core::error::Error;
use slate_kv_core::segment::{Segment, SegmentHeader};
use slate_kv_erasure::{RS_K, RS_M};

fn sample() -> SegmentHeader {
    SegmentHeader {
        magic: MAGIC_SEG,
        format_version: 1,
        seg_seq: 0x0102_0304_0506_0708,
        epoch: 0x1112_1314_1516_1718,
        minseq: 0x2122_2324_2526_2728,
        sealed: 0xFF,
        hdr_mac: [0xAB; 32],
    }
}

#[test]
fn header_len_is_the_documented_59_bytes() {
    // 1 magic + 1 version + 8 seg_seq + 8 epoch + 8 minseq + 1 sealed + 32 mac
    assert_eq!(SegmentHeader::LEN, 59);
    assert_eq!(1 + 1 + 8 + 8 + 8 + 1 + 32, SegmentHeader::LEN);
}

#[test]
fn encode_places_every_field_at_its_documented_offset() {
    let h = sample();
    let mut buf = [0u8; SegmentHeader::LEN];
    h.encode(&mut buf);

    assert_eq!(buf[0], MAGIC_SEG, "magic at byte 0");
    assert_eq!(buf[1], 1, "format_version at byte 1");
    // Little-endian throughout, matching the rest of the format.
    assert_eq!(&buf[2..10], &h.seg_seq.to_le_bytes(), "seg_seq at 2..10");
    assert_eq!(&buf[10..18], &h.epoch.to_le_bytes(), "epoch at 10..18");
    assert_eq!(&buf[18..26], &h.minseq.to_le_bytes(), "minseq at 18..26");
    assert_eq!(buf[26], 0xFF, "sealed flag at byte 26");
    assert_eq!(&buf[27..59], &h.hdr_mac, "hdr_mac at 27..59");
}

#[test]
fn decode_inverts_encode() {
    let h = sample();
    let mut buf = [0u8; SegmentHeader::LEN];
    h.encode(&mut buf);
    let got = SegmentHeader::decode(&buf).expect("valid header must decode");

    assert_eq!(got.magic, h.magic);
    assert_eq!(got.format_version, h.format_version);
    assert_eq!(got.seg_seq, h.seg_seq);
    assert_eq!(got.epoch, h.epoch);
    assert_eq!(got.minseq, h.minseq);
    assert_eq!(got.sealed, h.sealed);
    assert_eq!(got.hdr_mac, h.hdr_mac);
}

#[test]
fn decode_rejects_a_wrong_magic() {
    let mut buf = [0u8; SegmentHeader::LEN];
    sample().encode(&mut buf);
    buf[0] = MAGIC_SEG.wrapping_add(1);
    assert!(
        matches!(SegmentHeader::decode(&buf), Err(Error::FormatError)),
        "a foreign magic byte must be a format error, not a silently accepted header"
    );
}

#[test]
fn decode_rejects_an_erased_page() {
    // An erased NOR page reads as all-ones. Decoding one must fail rather than
    // yield a plausible-looking header with every field saturated.
    let buf = [0xFFu8; SegmentHeader::LEN];
    assert!(matches!(
        SegmentHeader::decode(&buf),
        Err(Error::FormatError)
    ));

    // And an all-zero page (a freshly zeroed image) likewise.
    let buf = [0x00u8; SegmentHeader::LEN];
    assert!(matches!(
        SegmentHeader::decode(&buf),
        Err(Error::FormatError)
    ));
}

#[test]
fn sealed_flag_distinguishes_open_from_sealed() {
    let mut h = sample();
    let mut buf = [0u8; SegmentHeader::LEN];

    h.sealed = 0xFF; // open: the erased state, so sealing only clears bits
    h.encode(&mut buf);
    assert_eq!(SegmentHeader::decode(&buf).unwrap().sealed, 0xFF);

    h.sealed = 0x00; // sealed
    h.encode(&mut buf);
    assert_eq!(SegmentHeader::decode(&buf).unwrap().sealed, 0x00);
}

#[test]
fn encode_is_deterministic() {
    let h = sample();
    let mut a = [0u8; SegmentHeader::LEN];
    let mut b = [0u8; SegmentHeader::LEN];
    h.encode(&mut a);
    h.encode(&mut b);
    assert_eq!(a, b);
}

#[test]
fn encode_overwrites_stale_bytes_completely() {
    // `encode` writes into a caller-supplied buffer that may hold a previous
    // header. Every one of the 59 bytes must be written, or a stale field
    // leaks into the new record.
    let mut buf = [0x5Au8; SegmentHeader::LEN];
    let h = SegmentHeader {
        magic: MAGIC_SEG,
        format_version: 0,
        seg_seq: 0,
        epoch: 0,
        minseq: 0,
        sealed: 0,
        hdr_mac: [0; 32],
    };
    h.encode(&mut buf);
    assert!(
        buf[1..].iter().all(|&b| b == 0),
        "a zero header left 0x5A bytes behind: {buf:?}"
    );
}

#[test]
fn data_and_parity_blocks_tile_the_segment_without_overlap() {
    let seg = Segment {
        start_addr: 0x8000,
        block_size: 4096,
    };

    // Data blocks are contiguous from the segment start.
    for i in 0..RS_K {
        assert_eq!(seg.data_block(i), 0x8000 + (i as u32) * 4096);
    }
    // Parity blocks follow immediately after the last data block.
    for j in 0..RS_M {
        assert_eq!(seg.parity_block(j), 0x8000 + ((RS_K + j) as u32) * 4096);
    }
    assert_eq!(
        seg.parity_block(0),
        seg.data_block(RS_K - 1) + 4096,
        "parity must start where data ends, with no gap"
    );

    // No address is claimed twice across the whole RS(n,k) stripe.
    let mut addrs: Vec<u32> = (0..RS_K).map(|i| seg.data_block(i)).collect();
    addrs.extend((0..RS_M).map(|j| seg.parity_block(j)));
    let mut sorted = addrs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        addrs.len(),
        "block addresses overlap: {addrs:?}"
    );
    assert_eq!(sorted.len(), RS_K + RS_M);
}

#[test]
fn block_addressing_is_offset_by_the_segment_start() {
    // Segments tile the log area above the reserved region, so addressing must
    // be relative to start_addr rather than to zero.
    let a = Segment {
        start_addr: 0,
        block_size: 256,
    };
    let b = Segment {
        start_addr: 1 << 20,
        block_size: 256,
    };
    for i in 0..RS_K {
        assert_eq!(b.data_block(i) - a.data_block(i), 1 << 20);
    }
}
