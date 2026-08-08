//! Measures the log-wrap halt: how far a sustained overwrite workload gets
//! before the append head runs off the end of the region, and what the segment
//! census looks like at that moment.
//!
//! This is the *before* instrument for the circular-allocator work. The failure
//! it captures is the one every ESP32-C3 run reproduces and that reviewers 1-5
//! all raised: the engine stops with `FlashFull` while nearly every segment is
//! free and erased, because garbage collection reclaims space that the append
//! head has no way to move back into.
//!
//! The workload overwrites a small key set, so the live set stays near zero and
//! the whole region is reclaimable. Any engine that can reuse reclaimed space
//! runs this forever; one that cannot halts after a single linear pass.
//!
//! Emits JSON on stdout for `docs/data/wrap_before.json`.

use slate_kv::{Db, KeySource, Options, Profile};

/// Region size for the probe. Small enough that a pass completes quickly,
/// large enough to hold many segments.
const CAPACITY: u32 = 1024 * 1024;

/// Distinct keys in the working set. The live set is therefore
/// `KEYS * (record overhead + key + value)` bytes and never grows, so
/// utilization stays near zero and every sealed segment is fully reclaimable.
const KEYS: u32 = 16;

/// Upper bound on the run. A working engine reaches this; the current one
/// halts long before.
const TARGET_OPS: u32 = 20_000;

fn main() {
    // Label the run so the before/after pair is self-describing on disk.
    let label = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "wrap".to_string());
    let path = std::path::Path::new("./target/wrap_probe_db");
    let _ = std::fs::remove_dir_all(path);
    std::fs::create_dir_all(path).unwrap();

    let db = Db::open(
        path,
        KeySource::Bytes([0x24; 32]),
        Options {
            capacity: CAPACITY,
            b_commit: 4,
            auto_b: false,
            staleness_budget_ms: 1000,
            n_keys: 256,
            profile: Profile::Pi,
            durability: slate_kv::file_flash::Durability::Full,
            ..Default::default()
        },
    )
    .unwrap();

    let val = [b'v'; 64];
    let mut halted_at: Option<u32> = None;
    let mut halt_error = String::new();

    for i in 0..TARGET_OPS {
        let key = format!("key{:05}", i % KEYS);
        match db.put(key.as_bytes(), &val) {
            Ok(()) => {}
            Err(e) => {
                halted_at = Some(i);
                halt_error = format!("{e:?}");
                break;
            }
        }
        if i % 256 == 0 {
            if let Err(e) = db.commit() {
                halted_at = Some(i);
                halt_error = format!("{e:?}");
                break;
            }
        }
    }
    let _ = db.commit();

    let st = db.stats();

    // Does the data survive the halt? A halt that also loses acknowledged
    // writes is a different and worse defect, so record this explicitly.
    let mut readable = 0u32;
    for k in 0..KEYS {
        let key = format!("key{k:05}");
        if let Ok(Some(v)) = db.get(key.as_bytes()) {
            if v == val {
                readable += 1;
            }
        }
    }

    let reached = halted_at.unwrap_or(TARGET_OPS);
    let pct_capacity_used = 100.0 * (reached as f64 * 192.0) / CAPACITY as f64;

    println!("{{");
    println!("  \"probe\": \"{label}\",");
    println!("  \"capacity_bytes\": {CAPACITY},");
    println!("  \"working_set_keys\": {KEYS},");
    println!("  \"target_ops\": {TARGET_OPS},");
    println!("  \"ops_completed\": {reached},");
    println!(
        "  \"halted\": {},",
        if halted_at.is_some() { "true" } else { "false" }
    );
    println!("  \"halt_error\": \"{halt_error}\",");
    println!(
        "  \"approx_region_passes\": {:.2},",
        pct_capacity_used / 100.0
    );
    println!("  \"segments_total\": {},", st.segments);
    println!("  \"segments_free_at_halt\": {},", st.segments_free);
    println!("  \"segments_sealed_at_halt\": {},", st.segments_sealed);
    println!("  \"keys_still_readable\": {readable},");
    println!("  \"keys_expected_readable\": {KEYS},");
    println!("  \"user_bytes\": {},", st.user_bytes);
    println!("  \"gc_bytes\": {},", st.gc_bytes);
    println!("  \"parity_bytes\": {},", st.parity_bytes);
    println!("  \"marker_bytes\": {},", st.marker_bytes);
    println!("  \"ckpt_bytes\": {},", st.ckpt_bytes);
    println!("  \"erases\": {},", st.erases);
    println!("  \"gc_scanned\": {},", st.gc_scanned);
    println!("  \"gc_relocated\": {},", st.gc_relocated);
    println!("  \"gc_open_failed\": {},", st.gc_open_failed);
    println!(
        "  \"write_amplification\": {}",
        st.write_amplification()
            .map(|w| format!("{w:.4}"))
            .unwrap_or_else(|| "null".into())
    );
    println!("}}");
}
