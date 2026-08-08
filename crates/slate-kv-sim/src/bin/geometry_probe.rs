//! Geometry portability probe.
//!
//! Drives the REAL engine through `sim_db::Db` against synthetic flash
//! geometries and records, per (page_size, block_size), whether the volume
//! mounts, whether records survive commit -> get, whether they survive a
//! remount, and whether compaction runs. The current format hardcodes a 4 KiB
//! erase block in `SEG_DATA_BYTES`, so this is the measurement that decides
//! which MCU families can host the engine unchanged.
//!
//! Emits CSV on stdout: one row per geometry.

use slate_kv_sim::sim_db::{Db, KeySource, Options, Profile};
use slate_kv_sim::{SimCounter, SimFlash};

/// Outcome of one geometry trial, in the order the engine would hit them.
#[derive(Debug)]
enum Outcome {
    OpenFailed(String),
    PutFailed(String),
    CommitFailed(String),
    GetMismatch(String),
    RemountFailed(String),
    RemountMismatch(String),
    CompactFailed(String),
    Ok,
}

impl Outcome {
    fn stage(&self) -> &'static str {
        match self {
            Outcome::OpenFailed(_) => "open",
            Outcome::PutFailed(_) => "put",
            Outcome::CommitFailed(_) => "commit",
            Outcome::GetMismatch(_) => "get",
            Outcome::RemountFailed(_) => "remount",
            Outcome::RemountMismatch(_) => "remount_get",
            Outcome::CompactFailed(_) => "compact",
            Outcome::Ok => "ok",
        }
    }
    fn detail(&self) -> String {
        match self {
            Outcome::OpenFailed(s)
            | Outcome::PutFailed(s)
            | Outcome::CommitFailed(s)
            | Outcome::GetMismatch(s)
            | Outcome::RemountFailed(s)
            | Outcome::RemountMismatch(s)
            | Outcome::CompactFailed(s) => s.replace(',', ";").replace('\n', " "),
            Outcome::Ok => String::new(),
        }
    }
}

/// Number of distinct keys written per trial. Small enough that every geometry
/// under test can hold them, large enough to span more than one page.
const N_RECORDS: usize = 24;

fn opts(capacity: u32) -> Options {
    Options {
        capacity,
        // Commit explicitly, so a failure is attributable to the commit path
        // rather than to an implicit flush inside `put`.
        b_commit: 1,
        auto_b: false,
        staleness_budget_ms: 1000,
        // 256 keys keeps the index (and therefore the checkpoint) small; the
        // point of the probe is flash geometry, not index sizing.
        n_keys: 256,
        profile: Profile::Esp32,
    }
}

fn key_bytes() -> KeySource {
    KeySource::Bytes([0x42; 32])
}

fn val_for(i: usize) -> Vec<u8> {
    // 40-byte values: with the 44-byte record overhead this puts several
    // records in a 256 B page and spans pages within one commit.
    let mut v = vec![0u8; 40];
    v[0] = i as u8;
    v[1] = (i >> 8) as u8;
    v
}

fn run_trial(page: usize, block: usize, capacity: u32) -> Outcome {
    let flash = SimFlash::new(capacity, page, block);
    let counter = SimCounter::new(1_000_000);

    let db = match Db::open(key_bytes(), opts(capacity), flash, counter) {
        Ok(db) => db,
        Err(b) => return Outcome::OpenFailed(format!("{:?}", b.0)),
    };

    for i in 0..N_RECORDS {
        let k = format!("k{i:04}");
        if let Err(e) = db.put(k.as_bytes(), &val_for(i)) {
            return Outcome::PutFailed(format!("{e:?}"));
        }
        if let Err(e) = db.commit() {
            return Outcome::CommitFailed(format!("i={i} {e:?}"));
        }
    }

    for i in 0..N_RECORDS {
        let k = format!("k{i:04}");
        match db.get(k.as_bytes()) {
            Ok(Some(v)) if v == val_for(i) => {}
            Ok(other) => {
                return Outcome::GetMismatch(format!("i={i} got {:?}", other.map(|v| v.len())))
            }
            Err(e) => return Outcome::GetMismatch(format!("i={i} {e:?}")),
        }
    }

    // Remount: the index must be reconstructible from flash alone. This is
    // where a checkpoint-region/segment-stride disagreement surfaces.
    let mut db = db;
    let (flash, counter) = db.take_flash_and_counter();
    let db = match Db::open(key_bytes(), opts(capacity), flash, counter) {
        Ok(db) => db,
        Err(b) => return Outcome::RemountFailed(format!("{:?}", b.0)),
    };
    for i in 0..N_RECORDS {
        let k = format!("k{i:04}");
        match db.get(k.as_bytes()) {
            Ok(Some(v)) if v == val_for(i) => {}
            Ok(other) => {
                return Outcome::RemountMismatch(format!("i={i} got {:?}", other.map(|v| v.len())))
            }
            Err(e) => return Outcome::RemountMismatch(format!("i={i} {e:?}")),
        }
    }

    if let Err(e) = db.compact() {
        return Outcome::CompactFailed(format!("{e:?}"));
    }

    Outcome::Ok
}

fn main() {
    // Erase-block sizes spanning the real hardware: 256 B / 512 B is small
    // SPI NOR sub-sector, 4 KiB is the ESP32/RP2040 SPI NOR sector, 2 KiB is
    // STM32L0 page, 1-2 KiB is STM32C0/F1 page, 128 KiB is an STM32F4 sector.
    let blocks = [256usize, 512, 1024, 2048, 4096, 8192, 16384, 65536, 131072];
    // Program granularities: 1/4/8 B are STM32-class word writes, 256 B is SPI
    // NOR page programming, 512 B is the format's MAX_PAGE_SIZE ceiling.
    let pages = [1usize, 4, 8, 256, 512];

    println!("page_size,block_size,capacity,data_base,seg_stride_ok,min_volume,stage,detail");
    for &block in &blocks {
        for &page in &pages {
            if page > block {
                continue;
            }
            let data_base = slate_kv_core::config::data_base_offset(block);
            let seg_stride_ok = 12 * block == slate_kv_core::config::SEG_BYTES;
            // Enough room for the reserved region plus several segments.
            let min_volume = data_base as u64 + 8 * slate_kv_core::config::SEG_BYTES as u64;
            let capacity = (min_volume.next_power_of_two() as u32).max(1 << 20);

            let outcome = std::panic::catch_unwind(|| run_trial(page, block, capacity))
                .unwrap_or(Outcome::OpenFailed("panic".into()));

            println!(
                "{page},{block},{capacity},{data_base},{seg_stride_ok},{min_volume},{},{}",
                outcome.stage(),
                outcome.detail()
            );
        }
    }
}
