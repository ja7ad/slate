//! Focused diagnostics for the two failure modes `geometry_probe` found.
//!
//! 1. `page_size < CM_LEN` (83): `Log::program_page` writes
//!    `min(data.len(), page_size)` bytes, so the commit marker is silently
//!    TRUNCATED and recovery can never verify it. Affects every part that
//!    programs in words (STM32: 4-8 B), not just exotic ones.
//! 2. `page_size == 512`: recovery stops partway through the record set.
//!    Sweep the record count to find where it turns over.

use slate_kv_sim::sim_db::{Db, KeySource, Options, Profile};
use slate_kv_sim::{SimCounter, SimFlash};

fn opts(capacity: u32) -> Options {
    Options {
        capacity,
        b_commit: 1,
        auto_b: false,
        staleness_budget_ms: 1000,
        n_keys: 256,
        profile: Profile::Esp32,
    }
}

fn key_bytes() -> KeySource {
    KeySource::Bytes([0x42; 32])
}

fn val_for(i: usize) -> Vec<u8> {
    let mut v = vec![0u8; 40];
    v[0] = i as u8;
    v[1] = (i >> 8) as u8;
    v
}

/// Writes `n` records, remounts, and returns how many survive.
fn survivors(page: usize, block: usize, capacity: u32, n: usize) -> Result<usize, String> {
    let flash = SimFlash::new(capacity, page, block);
    let counter = SimCounter::new(1_000_000);
    let db = Db::open(key_bytes(), opts(capacity), flash, counter)
        .map_err(|b| format!("open: {:?}", b.0))?;

    for i in 0..n {
        let k = format!("k{i:04}");
        db.put(k.as_bytes(), &val_for(i))
            .map_err(|e| format!("put {i}: {e:?}"))?;
        db.commit().map_err(|e| format!("commit {i}: {e:?}"))?;
    }

    let mut db = db;
    let (flash, counter) = db.take_flash_and_counter();
    let db = Db::open(key_bytes(), opts(capacity), flash, counter)
        .map_err(|b| format!("remount: {:?}", b.0))?;

    let mut ok = 0;
    for i in 0..n {
        let k = format!("k{i:04}");
        if let Ok(Some(v)) = db.get(k.as_bytes()) {
            if v == val_for(i) {
                ok += 1;
            }
        }
    }
    Ok(ok)
}

fn main() {
    println!("== CM_LEN vs page_size ==");
    println!(
        "CM_LEN={} MAX_PAGE_SIZE={}",
        slate_kv_core::config::CM_LEN,
        slate_kv_core::config::MAX_PAGE_SIZE
    );
    for page in [1usize, 4, 8, 16, 64, 83, 84, 128, 256, 512] {
        let truncated = page < slate_kv_core::config::CM_LEN;
        let r = survivors(page, 4096, 1 << 20, 8);
        println!(
            "page={page:4} marker_truncated={truncated:5} survivors_of_8={:?}",
            r
        );
    }

    println!();
    println!("== page=512 record-count sweep (block=4096, cap=1MiB) ==");
    for n in [1usize, 2, 4, 8, 12, 16, 17, 20, 24, 32] {
        println!("n={n:3} survivors={:?}", survivors(512, 4096, 1 << 20, n));
    }

    println!();
    println!("== page=512 with a larger volume (4 MiB) ==");
    for n in [16usize, 24, 32, 48] {
        println!("n={n:3} survivors={:?}", survivors(512, 4096, 4 << 20, n));
    }

    println!();
    println!("== page=256 control, varying block ==");
    for block in [256usize, 1024, 4096, 65536] {
        println!(
            "block={block:6} survivors_of_24={:?}",
            survivors(256, block, 4 << 20, 24)
        );
    }

    // Hypothesis: the survivor ceiling is not a page-size effect at all. With
    // b_commit=1 each commit programs data + XOR parity + 2 marker copies = 4
    // pages, so bytes-per-record = 4 * page_size. SEG_DATA_BYTES is 32768, so
    // the log fills ONE segment after 32768/(4*page) records: 16 at page=512,
    // 32 at page=256, 64 at page=128. If recovery only replays the first
    // segment, the ceiling should track that formula on EVERY page size --
    // including 256, which the probe reported as "ok" only because it never
    // wrote past the first segment.
    println!();
    println!("== segment-ceiling hypothesis: predicted vs measured ==");
    println!("SEG_DATA_BYTES={}", slate_kv_core::config::SEG_DATA_BYTES);
    for page in [128usize, 256, 512] {
        let per_record = 4 * page;
        let predicted = slate_kv_core::config::SEG_DATA_BYTES / per_record;
        // Write well past the predicted ceiling.
        let n = predicted * 3;
        println!(
            "page={page:4} bytes/record={per_record:5} predicted_ceiling={predicted:4} \
             wrote={n:4} survivors={:?}",
            survivors(page, 4096, 4 << 20, n)
        );
    }
}
