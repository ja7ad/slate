use slate_kv_core::chain::Chain;
use slate_kv_core::config::*;
use slate_kv_core::log::{HeadState, Log};
use slate_kv_core::recover::recover;
use slate_kv_sim::SimFlash;

use slate_kv_crypto::keys::{DeviceKey, KeySet};
use slate_kv_crypto::sealer::CryptoSealer;

fn main() {
    let mut flash = SimFlash::new(SEG_BYTES as u32, 256, 4096);
    let dk = DeviceKey([42; 32]);
    let mut sealer = CryptoSealer::new(KeySet::derive(&dk, 1));
    let mut buf = std::vec![0u8; 65536];

    let head = HeadState {
        seg_seq: 0,
        write_offset: 256,
        block_idx: 0,
        ..Default::default()
    };
    let mut chain = Chain::default();
    let mut log = Log::new(&mut buf, head);

    for i in 1..=3 {
        let key = format!("key{}", i).into_bytes();
        let val = format!("val{}", i).into_bytes();
        let _ = log
            .append(i, 1, OP_PUT, &key, &val, &mut sealer, &mut chain)
            .unwrap()
            .0;
    }
    log.commit(&mut flash, &mut sealer, &chain, 1, 3).unwrap();

    let mut rec_chain = Chain::default();
    let mut workspace = slate_kv_core::recover::RecoverWorkspace::new();
    let info = recover(
        &mut flash,
        &mut sealer,
        &mut rec_chain,
        1,
        0,
        &mut workspace,
        |_flash, _sealer, seq, off, _op, _key| {
            println!("Recovered seq: {} off: {}", seq, off);
        },
    )
    .unwrap();
    println!("committed_upto: {}", info.committed_upto);
}
