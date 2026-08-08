//! What is the engine's static RAM floor, and how far can configuration move
//! it?
//!
//! The per-chip verdict in docs/design/019 turns on this: parts with 8-32 KB of
//! SRAM are only reachable if the ~41 KB default footprint is mostly
//! *configuration* (index sizing) rather than *format* (fixed-size buffers).
//! This separates the two and prints the floor that no configuration can go
//! below.

use slate_kv_core::config as cfg;

fn main() {
    // Fixed cost: sized by format constants, identical on every device.
    let scratch = core::mem::size_of::<slate_kv_core::slate::ScratchWorkspace>();
    let segtable = core::mem::size_of::<slate_kv_core::gc::SegTable>();
    let engine = core::mem::size_of::<slate_kv_core::epoch::EngineState>();
    let recover_ws = core::mem::size_of::<slate_kv_core::recover::RecoverWorkspace>();

    println!("== fixed (format-determined) ==");
    println!("ScratchWorkspace   {scratch:>8}");
    println!("  of which: rec buffers are 2 x (REC_OVERHEAD + MAX_KEY_LEN + MAX_VAL_LEN)");
    println!(
        "            = 2 x ({} + {} + {}) = {}",
        cfg::REC_OVERHEAD,
        cfg::MAX_KEY_LEN,
        cfg::MAX_VAL_LEN,
        2 * (cfg::REC_OVERHEAD + cfg::MAX_KEY_LEN + cfg::MAX_VAL_LEN)
    );
    println!(
        "SegTable           {segtable:>8}  (MAX_SEGS = {})",
        cfg::MAX_SEGS
    );
    println!("EngineState        {engine:>8}");
    println!("RecoverWorkspace   {recover_ws:>8}  (transient, but must be allocatable)");
    let fixed = scratch + segtable + engine;
    println!(
        "fixed subtotal     {fixed:>8} = {:.1} KB",
        fixed as f64 / 1024.0
    );

    println!();
    println!("== configurable (index + log batch buffers) ==");
    println!("n_keys  idx_slots  index_B  ckpt_B   hot+cold_B   total_B    total_KB");
    for n_keys in [64usize, 128, 256, 512, 1024, 2048, 8192] {
        // Mirrors Db::open: n_buckets = max(n_keys, 2048)/0.95, rounded up to a
        // power of two, times BUCKET_SLOTS.
        let n_buckets = (n_keys.max(2048) as f64 / 0.95) as usize;
        let slots = n_buckets.next_power_of_two() * cfg::BUCKET_SLOTS;
        let index_b = slots * 4;
        let ckpt_b = cfg::ckpt_len_for_slots(slots);
        // Both db layers allocate 64 KiB hot + 64 KiB cold batch buffers.
        let batch_b = 2 * 65536;
        let total = fixed + index_b + ckpt_b + batch_b;
        println!(
            "{n_keys:6}  {slots:9}  {index_b:7}  {ckpt_b:7}  {batch_b:10}  {total:9}  {:8.1}",
            total as f64 / 1024.0
        );
    }

    println!();
    println!("== floor: smallest legal index, batch buffer sized to one commit ==");
    // The batch buffer only has to hold one commit's worth of records. A
    // b_commit=8 batch of 74-byte records is ~600 B, so 64 KiB is a host-side
    // convenience, not a requirement.
    let min_slots = 2048usize.next_power_of_two() * cfg::BUCKET_SLOTS;
    let min_index = min_slots * 4;
    let min_ckpt = cfg::ckpt_len_for_slots(min_slots);
    for batch in [1024usize, 2048, 4096] {
        let total = fixed + min_index + min_ckpt + 2 * batch;
        println!(
            "batch={batch:5} -> {total:7} B = {:.1} KB  (index {min_index} + ckpt {min_ckpt})",
            total as f64 / 1024.0
        );
    }

    println!();
    println!("== what a 'tiny' profile would need from the FORMAT ==");
    // If MAX_VAL_LEN and MAX_SEGS were reduced, the fixed cost falls too.
    let tiny_scratch = 2 * (cfg::REC_OVERHEAD + 64 + 128) + 2 * (64 + 128) + cfg::MAX_PAGE_SIZE;
    println!(
        "ScratchWorkspace with MAX_KEY_LEN=64, MAX_VAL_LEN=128: ~{tiny_scratch} B \
         (vs {scratch} B today)"
    );
    println!(
        "SegTable with MAX_SEGS=32: ~{} B (vs {segtable} B today)",
        segtable / 8
    );
}
