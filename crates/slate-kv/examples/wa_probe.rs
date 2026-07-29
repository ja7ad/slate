// Does the seal-thrash guard stop the WA climb near exhaustion?
fn main() {
    let p = std::path::Path::new("/tmp/wa/thrash");
    let _ = std::fs::remove_dir_all(p);
    std::fs::create_dir_all(p).unwrap();
    let opts = slate_kv::Options { capacity: 2*1024*1024, b_commit: 8, auto_b: false,
        staleness_budget_ms: 1000, n_keys: 4096, profile: slate_kv::Profile::Pi,
        durability: slate_kv::file_flash::Durability::Full };
    let db = slate_kv::Db::open(p, slate_kv::KeySource::Bytes([0x24;32]), opts).unwrap();
    let val = [b'v'; 16];
    let mut last_report = 0u32;
    for i in 0..20000u32 {
        if let Err(e) = db.put(b"async_test_key", &val) {
            let s = db.stats();
            println!("halt at {i}: {e:?}");
            println!("  epoch={} erases={} ckpt={} WA={:?} sealed={} free={}",
                db.epoch(), s.erases, s.ckpt_bytes, s.write_amplification(), s.segments_sealed, s.segments_free);
            println!("  gc scanned={} relocated={} freed={}", s.gc_scanned, s.gc_relocated, s.gc_segments_freed);
            break;
        }
        if i - last_report >= 2000 {
            last_report = i;
            let s = db.stats();
            println!("[{i}] epoch={} erases={} ckpt={} WA={:.4} sealed={} free={} freed={}",
                db.epoch(), s.erases, s.ckpt_bytes, s.write_amplification().unwrap_or(0.0),
                s.segments_sealed, s.segments_free, s.gc_segments_freed);
        }
    }
}
