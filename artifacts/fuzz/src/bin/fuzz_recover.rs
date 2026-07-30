#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // arbitrary flash images -> mount: must return Ok/Tampered/Rollback/TornTail
});
