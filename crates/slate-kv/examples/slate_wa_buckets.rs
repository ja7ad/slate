//! Paper measurement: write-amplification byte-bucket decomposition.
//!
//! One CSV row per `b_commit` at ESP32-C3 flash geometry (2 MiB SLATE region,
//! 256 B pages, 4 KiB erase blocks), reporting every bucket the engine
//! attributes — user, GC relocation, RS/XOR parity, commit markers, checkpoints
//! — plus the GC observability counters. The paper's WA table and its stacked
//! composition figure are both generated from this one file, so they cannot
//! disagree.
//!
//! `cargo run --release -p slate-kv --example slate_wa_buckets`

use slate_kv::{Db, KeySource, Options, Profile};

const CAPACITY: u32 = 2 * 1024 * 1024; // targets/esp32: SLATE_FLASH_LEN
const VAL_LEN: usize = 100;
const N_DISTINCT: usize = 256;
const N_OPS: usize = 6000;

fn main() {
    let root = std::env::temp_dir().join(format!("slate_wa_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    println!("# SLATE paper measurement: write-amplification byte buckets");
    println!(
        "# geometry=esp32c3 capacity={CAPACITY} page=256 block=4096 \
         val_len={VAL_LEN} n_distinct_keys={N_DISTINCT} n_ops={N_OPS} \
         durability=Full compaction=explicit"
    );
    println!(
        "b_commit,ops_accepted,acked_seq,live_keys,commits,wakes,\
         user_bytes,gc_bytes,parity_bytes,marker_bytes,ckpt_bytes,\
         total_bytes,erases,gc_scanned,gc_relocated,gc_open_failed,\
         gc_segments_freed,segments,segments_sealed,segments_free,\
         hot_head,cold_head,seg_end,wa"
    );

    for &b in &[1u32, 2, 4, 8, 16, 27, 32, 64, 128] {
        let dir = root.join(format!("b{b}"));
        std::fs::create_dir_all(&dir).unwrap();

        let opts = Options {
            capacity: CAPACITY,
            b_commit: b,
            auto_b: false,
            staleness_budget_ms: 1000,
            n_keys: 2048,
            profile: Profile::Esp32,
            durability: slate_kv::file_flash::Durability::Full,
            ..Default::default()
        };
        let db = Db::open(&dir, KeySource::Bytes([0x42; 32]), opts).unwrap();

        let val = vec![0xA5u8; VAL_LEN];
        let mut ops = 0usize;
        for i in 0..N_OPS {
            let key = format!("sensor_{:06}", i % N_DISTINCT);
            if db.put(key.as_bytes(), &val).is_err() {
                break;
            }
            ops = i + 1;
        }
        let _ = db.commit();
        let _ = db.compact();

        let s = db.stats();
        let wa = match s.write_amplification() {
            Some(w) => w,
            None => {
                eprintln!("b_commit={b}: nothing measured");
                continue;
            }
        };
        println!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{:.4}",
            b,
            ops,
            db.acked_seq(),
            db.len(),
            s.commits,
            s.wakes,
            s.user_bytes,
            s.gc_bytes,
            s.parity_bytes,
            s.marker_bytes,
            s.ckpt_bytes,
            s.flash_bytes(),
            s.erases,
            s.gc_scanned,
            s.gc_relocated,
            s.gc_open_failed,
            s.gc_segments_freed,
            s.segments,
            s.segments_sealed,
            s.segments_free,
            s.hot_head,
            s.cold_head,
            s.seg_end,
            wa
        );
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
    let _ = std::fs::remove_dir_all(&root);
}
