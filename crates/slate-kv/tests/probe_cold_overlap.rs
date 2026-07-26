//! Probe: db.rs initialises BOTH log heads at write_offset 0, but recovery only
//! repositions log_hot (`slate.log_hot.head.write_offset = rec_info.head_pos`).
//! data_base_offset(4096) = 90112, so the cold log — which carries tombstones
//! from delete() — starts writing at byte 0, inside the reserved + checkpoint
//! region (blocks 0..22).

use slate_kv::{Db, KeySource, Options};

#[test]
fn probe_cold_log_writes_below_data_base() {
    let base = slate_kv_core::config::data_base_offset(4096);
    println!("PROBE data_base_offset(4096)={base}");
    println!(
        "PROBE ckpt_region_blocks={}..{}",
        slate_kv_core::config::CKPT_BASE_BLOCK,
        base / 4096
    );

    let dir = std::env::temp_dir().join(format!("slate_probe_cold_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    fn mk() -> Options {
        Options {
            capacity: 4 * 1024 * 1024,
            n_keys: 2048,
            ..Default::default()
        }
    }

    let mut delete_errors = 0usize;
    let mut deletes_ok = 0usize;
    {
        let db = Db::open(&dir, KeySource::Bytes([41u8; 32]), mk()).unwrap();
        // Insert then delete many keys so the cold log grows.
        for i in 0..3000 {
            let k = format!("ck{:05}", i);
            db.put(k.as_bytes(), b"payload_data_here").unwrap();
        }
        db.commit().unwrap();
        for i in 0..3000 {
            let k = format!("ck{:05}", i);
            match db.delete(k.as_bytes()) {
                Ok(()) => deletes_ok += 1,
                Err(_) => {
                    delete_errors += 1;
                    break;
                }
            }
        }
        db.commit().ok();
        println!("PROBE deletes_ok={deletes_ok} delete_errors={delete_errors}");
        println!("PROBE len_after_deletes={}", db.len());
    }

    // Inspect raw flash: is anything written below data_base outside the
    // checkpoint slots? Read the first bytes of the device.
    let flash_bytes = std::fs::read(dir.join("slate.img"))
        .or_else(|_| std::fs::read(dir.join("flash.bin")))
        .or_else(|_| {
            // find whatever file the Db created
            let mut found = Err(std::io::Error::other("none"));
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.metadata().map(|m| m.len() > 1024 * 1024).unwrap_or(false) {
                    found = std::fs::read(&p);
                    break;
                }
            }
            found
        });

    if let Ok(bytes) = flash_bytes {
        let non_erased_below_base = bytes[..base as usize]
            .iter()
            .filter(|&&b| b != 0xFF)
            .count();
        // Blocks 0..CKPT_BASE_BLOCK are reserved and should be untouched.
        let reserved_end = (slate_kv_core::config::CKPT_BASE_BLOCK * 4096) as usize;
        let non_erased_reserved = bytes[..reserved_end].iter().filter(|&&b| b != 0xFF).count();
        println!("PROBE non_erased_bytes_below_data_base={non_erased_below_base}");
        println!("PROBE non_erased_bytes_in_reserved_blocks_0_2={non_erased_reserved}");
    } else {
        println!("PROBE could_not_read_flash_image");
    }

    // Reopen: does recovery still work?
    match Db::open(&dir, KeySource::Bytes([41u8; 32]), mk()) {
        Ok(db) => println!("PROBE reopen=OK len={} epoch={}", db.len(), db.epoch()),
        Err(e) => println!("PROBE reopen=ERR {e:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}
