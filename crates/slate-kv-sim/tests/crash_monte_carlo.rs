use slate_kv_core::chain::Chain;
use slate_kv_core::config::*;
use slate_kv_core::log::{HeadState, Log};
use slate_kv_core::recover::recover;
use slate_kv_sim::{Crash, SimFlash};

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use slate_kv_crypto::keys::{DeviceKey, KeySet};
use slate_kv_crypto::sealer::CryptoSealer;

#[test]
fn test_crash_monte_carlo() {
    // Run N=100 iterations for the unit test context. Real CI might run 5000.
    for seed in 0..100 {
        let mut rng = SmallRng::seed_from_u64(seed);
        let mut flash = SimFlash::new(4096 * 32, 256, 4096);
        let dev_key = DeviceKey([0; 32]);
        let keyset = KeySet::derive(&dev_key, 1);
        let mut sealer = CryptoSealer::new(keyset);

        let mut log_buf = [0u8; 65536];
        let mut log = Log::new(
            &mut log_buf,
            HeadState {
                seg_seq: 1,
                write_offset: 0,
                block_idx: 0,
            ..Default::default()
            },
        );

        // 1. Write batch 1
        let mut chain = Chain::anchor(1, &[0u8; 32]);
        let mut last_ticket = 0;
        for i in 1..=3 {
            let key = format!("key{}", i).into_bytes();
            let val = format!("val{}", i).into_bytes();
            last_ticket = log
                .append(i, 1, OP_PUT, &key, &val, &mut sealer, &mut chain)
                .unwrap()
                .0;
        }
        log.commit(&mut flash, &mut sealer, &chain, 1, 3).unwrap();
        let acked = last_ticket;

        // Write another batch that will crash
        for i in 4..=5 {
            let key = format!("key{}", i).into_bytes();
            let val = format!("val{}", i).into_bytes();
            let _ = log
                .append(i, 1, OP_PUT, &key, &val, &mut sealer, &mut chain)
                .unwrap();
        }

        // Setup crash at random byte inside commit
        let op_idx = rng.gen_range(0..5);
        let byte_in_op = rng.gen_range(0..256);
        flash.power.crash = Crash::AtByte {
            op_index: flash.power.current_op + op_idx,
            byte_in_op,
        };

        let _ = log.commit(&mut flash, &mut sealer, &chain, 1, 5);

        // 2. Recover
        let mut recovered_seqs = std::vec::Vec::new();
        let mut rec_chain = Chain::anchor(1, &[0u8; 32]);
        let mut workspace = slate_kv_core::recover::RecoverWorkspace::new();
        let info = recover(
            &mut flash,
            &mut sealer,
            &mut rec_chain,
            1,
            0,
            &mut workspace,
            |_flash, _sealer, seq, _off, _op, _key| recovered_seqs.push(seq),
        )
        .unwrap();

        // 3. Verify exactly acknowledged prefix or fully committed batch
        assert!(
            info.committed_upto == acked || info.committed_upto == 5,
            "Seed {} failed: recovered seq {} neither {} nor 5",
            seed,
            info.committed_upto,
            acked
        );

        let expected_seqs: std::vec::Vec<u64> = (1..=info.committed_upto).collect();
        assert_eq!(
            recovered_seqs, expected_seqs,
            "Seed {} failed: seqs mismatch",
            seed
        );
    }
}
