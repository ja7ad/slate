//! Checkpoint capacity must cover every index the public API will allocate.
//!
//! The whole index is serialized into a single checkpoint slot, so a
//! configuration whose index does not fit does not degrade gracefully: every
//! epoch seal fails and the database runs with no checkpoints at all — mount
//! becomes unbounded and GC never reclaims. `Db::open` must therefore reject
//! such a configuration up front rather than let it fail later.

use slate_kv::{Db, KeySource, Options};
use slate_kv_core::config::{
    ckpt_len_for_slots, index_serialized_len, max_index_slots, BUCKET_SLOTS, MAX_CKPT_LEN,
};

/// Mirrors the arena sizing in `slate-kv/src/db.rs::open`.
fn arena_slots_for(n_keys: usize) -> usize {
    let n_buckets = (n_keys.max(2048) as f64 / 0.95) as usize;
    n_buckets.next_power_of_two() * BUCKET_SLOTS
}

#[test]
fn checkpoint_holds_every_accepted_index() {
    for n_keys in [2048usize, 4096, 8192] {
        let slots = arena_slots_for(n_keys);
        assert!(
            slots <= max_index_slots(),
            "n_keys={n_keys} needs {slots} slots, above the {} the format allows",
            max_index_slots()
        );
        assert!(
            ckpt_len_for_slots(slots) as u32 <= MAX_CKPT_LEN,
            "n_keys={n_keys}: checkpoint of {} B exceeds MAX_CKPT_LEN {MAX_CKPT_LEN}",
            ckpt_len_for_slots(slots)
        );
        assert!(
            index_serialized_len(slots) < ckpt_len_for_slots(slots),
            "n_keys={n_keys}: serialized index does not leave room for header and tag"
        );
    }
}

/// The default configuration must be openable — a default that cannot
/// checkpoint would silently disable the design's core guarantee.
#[test]
fn default_options_open_successfully() {
    let dir = std::env::temp_dir().join(format!("slate_ckpt_size_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir, KeySource::Bytes([3u8; 32]), Options::default())
        .expect("default Options must open");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An index too large for the checkpoint format must be refused at `open`,
/// with a message naming the limit — not accepted and then found unusable at
/// the first epoch seal.
#[test]
fn oversized_n_keys_is_rejected_at_open() {
    let dir = std::env::temp_dir().join(format!("slate_ckpt_over_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let o = Options {
        n_keys: max_index_slots(), // far past what a checkpoint can hold
        ..Default::default()
    };
    let r = Db::open(&dir, KeySource::Bytes([4u8; 32]), o);
    assert!(r.is_err(), "an unsupportable n_keys must not open");
    let _ = std::fs::remove_dir_all(&dir);
}
