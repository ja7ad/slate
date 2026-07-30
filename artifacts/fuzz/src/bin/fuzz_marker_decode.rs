#![no_main]

use libfuzzer_sys::fuzz_target;
// Assuming marker struct exists, we will stub or parse.
// We can just use the CM marker parse.

fuzz_target!(|data: &[u8]| {
    // Stub
});
