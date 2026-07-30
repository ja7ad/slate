#![no_main]

use libfuzzer_sys::fuzz_target;
use slate_kv_core::record::RecordHeader;

fuzz_target!(|data: &[u8]| {
    if data.len() >= 28 {
        let mut buf = [0u8; 28];
        buf.copy_from_slice(&data[..28]);
        let _ = RecordHeader::decode(&buf);
    }
});
