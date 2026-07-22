use slate_sim::{SimFlash, Crash};
use slate_core::log::{Log, Sealer, CmFields, HeadState};
use slate_core::recover::recover;
use slate_core::config::*;
use slate_core::error::Error;
use rand::{Rng, SeedableRng};
use rand::rngs::SmallRng;

pub struct StubSealer;

impl Sealer for StubSealer {
    fn seal_record(&mut self, _hdr: &[u8; REC_HDR_LEN], plain_kv: &[u8], ct_tag_out: &mut [u8]) {
        let n = plain_kv.len();
        ct_tag_out[..n].copy_from_slice(plain_kv);
        let mut xor = 0u8;
        for &b in plain_kv { xor ^= b; }
        ct_tag_out[n..n+TAG_LEN].fill(xor);
    }
    fn open_record(&mut self, _hdr: &[u8; REC_HDR_LEN], ct_tag: &[u8], plain_out: &mut [u8]) -> Result<(), Error> {
        let n = ct_tag.len() - TAG_LEN;
        plain_out[..n].copy_from_slice(&ct_tag[..n]);
        let mut xor = 0u8;
        for &b in &ct_tag[..n] { xor ^= b; }
        if ct_tag[n] != xor { return Err(Error::Tampered); }
        Ok(())
    }
    fn chain_fold(&mut self, _record_bytes: &[u8]) {}
    fn commit_marker(&mut self, seq_max: u64, epoch: u64) -> [u8; CM_LEN] {
        let mut cm = [0xFF; CM_LEN];
        cm[0] = MAGIC_CM;
        cm[1..9].copy_from_slice(&seq_max.to_le_bytes());
        cm[9..17].copy_from_slice(&epoch.to_le_bytes());
        cm[17..49].fill(0xAA);
        cm[49..81].fill(0xBB);
        cm
    }
    fn verify_marker(&self, cm: &[u8; CM_LEN]) -> Result<CmFields, Error> {
        if cm[0] != MAGIC_CM { return Err(Error::FormatError); }
        let seq_max = u64::from_le_bytes(cm[1..9].try_into().unwrap());
        let epoch = u64::from_le_bytes(cm[9..17].try_into().unwrap());
        Ok(CmFields {
            magic: cm[0], seq_max, epoch, chi: [0xAA; 32], tau_cm: [0xBB; 32]
        })
    }
}

#[test]
fn test_crash_monte_carlo() {
    let mut rng = SmallRng::seed_from_u64(42);
    // Run N=100 iterations for the unit test context. Real CI might run 5000.
    for seed in 0..100 {
        let mut flash = SimFlash::new(SEG_BYTES as u32, 256, 4096);
        let mut sealer = StubSealer;
        let mut buf = std::vec![0u8; 65536]; // Batch buffer

        // 1. Initial write of some records
        let head = HeadState { seg_seq: 0, write_offset: 256, block_idx: 0 }; // After segment header
        let mut log = Log::new(&mut buf, 1, 0, 1, head);

        let mut last_ticket = 0;
        
        // Write 3 records
        for i in 1..=3 {
            let key = format!("key{}", i).into_bytes();
            let val = format!("val{}", i).into_bytes();
            last_ticket = log.append(OP_PUT, &key, &val, &mut sealer).unwrap();
        }
        log.commit(&mut flash, &mut sealer).unwrap();
        let acked = last_ticket;

        // Write another batch that will crash
        for i in 4..=5 {
            let key = format!("key{}", i).into_bytes();
            let val = format!("val{}", i).into_bytes();
            let _ = log.append(OP_PUT, &key, &val, &mut sealer).unwrap();
        }

        // Setup crash at random byte inside commit
        let op_idx = rng.gen_range(0..5);
        let byte_in_op = rng.gen_range(0..256);
        flash.power.crash = Crash::AtByte { op_index: flash.power.current_op + op_idx, byte_in_op };

        let _ = log.commit(&mut flash, &mut sealer);

        // 2. Recover
        let mut recovered_seqs = std::vec::Vec::new();
        let info = recover(&mut flash, &mut sealer, |seq| recovered_seqs.push(seq)).unwrap();

        // 3. Verify exactly acknowledged prefix or fully committed batch
        assert!(info.committed_upto == acked || info.committed_upto == 5, "Seed {} failed: recovered seq {} neither {} nor 5", seed, info.committed_upto, acked);
        
        let expected_seqs: std::vec::Vec<u64> = (1..=info.committed_upto).collect();
        assert_eq!(recovered_seqs, expected_seqs, "Seed {} failed: seqs mismatch", seed);
    }
}
