#![no_main]

use libfuzzer_sys::fuzz_target;
use slate_core::checkpoint::CheckpointHeader;

fuzz_target!(|data: &[u8]| {
    if data.len() >= 44 {
        let mut buf = [0u8; 44];
        buf.copy_from_slice(&data[..44]);
        let _ = CheckpointHeader::decode(&buf);
    }
});
