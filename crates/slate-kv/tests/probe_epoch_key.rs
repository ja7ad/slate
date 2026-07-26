//! Probe: after roll_epoch, can a record sealed in the PREVIOUS epoch still be
//! opened? The record header carries no epoch field, and KeySet holds only the
//! current epoch's k_rec_e, so the answer determines whether epoch rolls would
//! destroy readability of already-committed records.

use slate_kv_core::config::{MAGIC_REC, OP_PUT, REC_HDR_LEN, TAG_LEN};
use slate_kv_core::log::Sealer;
use slate_kv_core::record::RecordHeader;
use slate_kv_crypto::keys::{DeviceKey, KeySet};
use slate_kv_crypto::sealer::CryptoSealer;

#[test]
fn probe_prev_epoch_record_readable_after_roll() {
    let dk = DeviceKey([9u8; 32]);
    let ks = KeySet::derive(&dk, 1);
    let mut sealer = CryptoSealer::new(ks);

    let key = b"alpha";
    let val = b"payload";
    let mut plain = Vec::new();
    plain.extend_from_slice(key);
    plain.extend_from_slice(val);

    let mut hdr = RecordHeader {
        magic: MAGIC_REC,
        seq: 42,
        op: OP_PUT,
        fp: 1,
        klen: key.len() as u16,
        vlen: val.len() as u16,
        nonce: [0u8; 12],
    };
    hdr.nonce[0..8].copy_from_slice(&42u64.to_le_bytes());
    let mut hdr_bytes = [0u8; REC_HDR_LEN];
    hdr.encode(&mut hdr_bytes);

    // Seal in epoch 1.
    let mut ct = vec![0u8; plain.len() + TAG_LEN];
    sealer.seal_record(&hdr_bytes, &plain, &mut ct);

    // Sanity: readable in the same epoch.
    let mut out_same = vec![0u8; plain.len()];
    let same_ok = sealer.open_record(&hdr_bytes, &ct, &mut out_same).is_ok();
    println!("PROBE same_epoch_open_ok={same_ok}");
    assert!(same_ok, "record must open in its own epoch");

    // Now roll to epoch 2, exactly as seal_epoch does.
    sealer.roll_epoch(2);

    let mut out_after = vec![0u8; plain.len()];
    let after_ok = sealer.open_record(&hdr_bytes, &ct, &mut out_after).is_ok();
    println!("PROBE after_roll_open_ok={after_ok}");

    assert!(
        after_ok,
        "record sealed in epoch 1 became UNREADABLE after roll_epoch(2): \
         the header has no epoch discriminator and KeySet keeps only k_rec_e \
         for the current epoch"
    );
}
