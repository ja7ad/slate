use slate_kv_core::config::*;
use slate_kv_core::log::{HeadState, Log, Sealer};
use slate_kv_crypto::keys::{DeviceKey, KeySet};
use slate_kv_crypto::sealer::CryptoSealer;
use slate_kv_erasure::matrix::cauchy_row;
use slate_kv_erasure::reconstruct::{reconstruct, BlockSet};
use slate_kv_erasure::{PAGE_SIZE, RS_K, RS_M, RS_N};

#[test]
fn test_reed_solomon_database_recovery() {
    let dev_key = DeviceKey([0x42; 32]);
    let keyset = KeySet::derive(&dev_key, 1);
    let mut sealer = CryptoSealer::new(keyset);

    // 1. Prepare 8 Data Blocks (K=8) and 4 Parity Blocks (M=4)
    let mut stripe = [[0u8; PAGE_SIZE]; RS_N];

    // Populate data blocks with real encrypted SLATE records
    let mut log_buf = [0u8; 4096];
    let mut chain = slate_kv_core::chain::Chain::anchor(1, &[0u8; 32]);
    let mut log = Log::<'_, slate_kv_sim::SimFlash>::new(
        &mut log_buf,
        HeadState {
            seg_seq: 1,
            write_offset: 0,
            block_idx: 0,
            ..Default::default()
        },
    );

    let records = [
        ("device_config", "active1"),
        ("device_config2", "active2"),
        ("device_config3", "active3"),
        ("device_config4", "active4"),
    ];

    for (seq, (key, val)) in records.iter().enumerate() {
        log.append(
            (seq + 1) as u64,
            1,
            OP_PUT,
            key.as_bytes(),
            val.as_bytes(),
            &mut sealer,
            &mut chain,
        )
        .unwrap();
    }

    // Store encrypted log data into data blocks
    let data_len = log.batch.data().len();
    for (i, block) in stripe.iter_mut().take(RS_K).enumerate() {
        let start = i * PAGE_SIZE;
        if start < data_len {
            let end = core::cmp::min(start + PAGE_SIZE, data_len);
            block[..end - start].copy_from_slice(&log.batch.data()[start..end]);
            if end - start < PAGE_SIZE {
                block[end - start..].fill(0xFF);
            }
        } else {
            block.fill(0xFF);
        }
    }

    // 2. Compute RS(12,8) Cauchy Parity for Blocks 8..12
    for j in 0..RS_M {
        let p_idx = RS_K + j;
        let row = cauchy_row(j);
        for i in 0..RS_K {
            let c = row[i];
            let d_row = stripe[i];
            for (o, &d) in stripe[p_idx].iter_mut().zip(&d_row) {
                *o ^= slate_kv_erasure::gf::gf_mul(c, d);
            }
        }
    }
    let encoded_stripe = stripe;

    // 3. Simulate Severe Storage Erasure: Wipe 4 Blocks (Max Fault Tolerance M=4)
    // Wipe Data Block 0, Data Block 2, Data Block 5, and Parity Block 10
    let mut erased = BlockSet::new();
    let erased_indices = [0, 2, 5, 10];
    for &idx in &erased_indices {
        erased.insert(idx);
        stripe[idx] = [0u8; PAGE_SIZE]; // Corrupt / Wipe block completely
    }

    // Verify blocks are indeed corrupted before reconstruction
    assert_ne!(stripe[0], encoded_stripe[0]);
    assert_ne!(stripe[2], encoded_stripe[2]);

    // 4. Perform Reed-Solomon Erasure Reconstruction
    reconstruct(&mut stripe, &erased).expect("RS reconstruction failed");

    // 5. Verify 100% Byte-for-Byte Exact Reconstruction
    for idx in 0..RS_N {
        assert_eq!(
            stripe[idx], encoded_stripe[idx],
            "Block {} failed RS reconstruction",
            idx
        );
    }

    // 6. Verify Decryption & Integrity of Reconstructed Data Records
    let mut reconstructed_data = [0u8; 512];
    reconstructed_data[..256].copy_from_slice(&stripe[0]);
    reconstructed_data[256..512].copy_from_slice(&stripe[1]);

    let mut off = 0;
    let expected = [
        ("device_config", "active1"),
        ("device_config2", "active2"),
        ("device_config3", "active3"),
        ("device_config4", "active4"),
    ];

    for (exp_key, exp_val) in expected {
        let mut hdr_bytes = [0u8; REC_HDR_LEN];
        hdr_bytes.copy_from_slice(&reconstructed_data[off..off + REC_HDR_LEN]);

        let hdr = slate_kv_core::record::RecordHeader::decode(&hdr_bytes).unwrap();
        let total_len = REC_OVERHEAD + hdr.klen as usize + hdr.vlen as usize;

        let mut plain_out = [0u8; 512];
        sealer
            .open_record(
                &hdr_bytes,
                &reconstructed_data[off + REC_HDR_LEN..off + total_len],
                &mut plain_out,
            )
            .unwrap();

        let klen = hdr.klen as usize;
        let vlen = hdr.vlen as usize;
        assert_eq!(&plain_out[..klen], exp_key.as_bytes());
        assert_eq!(&plain_out[klen..klen + vlen], exp_val.as_bytes());

        off += total_len;
    }
}
