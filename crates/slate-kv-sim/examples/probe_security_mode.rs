//! Probe: what `SecurityMode` does each counter backend actually report, on a
//! freshly formatted volume versus a remount?
//!
//! `mount` derives the mode from `CounterKind`, but the genesis-format branch
//! (taken when no valid checkpoint exists yet) does not — it constructs
//! `EngineState` with a literal. This separates the two so the reported mode can
//! be attributed to the counter rather than to the code path.
//!
//! Run: cargo run --release -p slate-kv-sim --example probe_security_mode

use slate_kv_hal::MonotonicCounter;
use slate_kv_sim::sim_db::{Db as SimDb, KeySource as SimKey, Options as SimOpts, Profile};
use slate_kv_sim::{SimCounter, SimFlash};

fn main() {
    let opts = SimOpts {
        capacity: 1024 * 1024,
        b_commit: 8,
        auto_b: false,
        staleness_budget_ms: 1_000_000,
        n_keys: 1024,
        profile: Profile::Pi,
    };

    let counter = SimCounter::new(100_000);
    println!("sim_counter_kind,{:?}", MonotonicCounter::kind(&counter));

    let flash = SimFlash::new(opts.capacity, 256, 4096);
    let mut db =
        SimDb::open(SimKey::Bytes([5u8; 32]), opts.clone(), flash, counter).expect("genesis open");
    println!("mode_on_genesis_format,{:?}", db.security_mode());
    println!("epoch_on_genesis_format,{}", db.epoch());
    for i in 0..24 {
        db.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    db.commit().unwrap();
    let (flash, counter) = db.take_flash_and_counter();

    // Second open: a valid checkpoint exists, so this goes through the real
    // `mount` path where the mode is derived from `CounterKind`.
    let db2 = SimDb::open(SimKey::Bytes([5u8; 32]), opts, flash, counter).expect("remount");
    println!("mode_on_remount,{:?}", db2.security_mode());
    println!("epoch_on_remount,{}", db2.epoch());
    println!("keys_on_remount,{}", db2.len());
}
