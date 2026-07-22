use slate_core::chain::Chain;
use slate_core::config::*;
use slate_core::log::{HeadState, Log};
use slate_core::recover::recover;
use slate_sim::{Crash, SimFlash};

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use slate_crypto::keys::{DeviceKey, KeySet};
use slate_crypto::sealer::CryptoSealer;

#[test]
fn test_crash_monte_carlo() {
    let mut rng = SmallRng::seed_from_u64(42);
    // Run N=100 iterations for the unit test context. Real CI might run 5000.
    for seed in 0..100 {
        let mut flash = SimFlash::new(SEG_BYTES as u32, 256, 4096);
        let dk = DeviceKey([42; 32]);
        let mut sealer = CryptoSealer::new(KeySet::derive(&dk, 1));
        let mut buf = std::vec![0u8; 65536]; // Batch buffer

        // 1. Initial write of some records
        let head = HeadState {
            seg_seq: 0,
            write_offset: 256,
            block_idx: 0,
        }; // After segment header
        let mut chain = Chain::default();
        let mut log = Log::new(&mut buf, head);

        let mut last_ticket = 0;

        // Write 3 records
        for i in 1..=3 {
            let key = format!("key{}", i).into_bytes();
            let val = format!("val{}", i).into_bytes();
            last_ticket = log
                .append(i, OP_PUT, &key, &val, &mut sealer, &mut chain)
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
                .append(i, OP_PUT, &key, &val, &mut sealer, &mut chain)
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
        let mut rec_chain = Chain::default();
        let info = recover(
            &mut flash,
            &mut sealer,
            &mut rec_chain,
            1,
            |seq, _off, _op, _key| recovered_seqs.push(seq),
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
