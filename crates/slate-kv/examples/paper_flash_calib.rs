//! Paper measurement: cost of a single `FileFlash::program` under each
//! `Durability` mode, measured at the HAL level with no engine above it.
//!
//! Why this exists. In `paper_throughput.rs` the two `Durability` modes come
//! out indistinguishable, which contradicts the expectation that `OsCache`
//! (documented as benchmark-only, i.e. cheaper) should be much faster than
//! `Full` (`F_FULLFSYNC`). Rather than explain that away, this isolates one
//! `program` call so the barrier cost is measured directly and the throughput
//! numbers can be attributed to a page count times a per-page cost.
//!
//! It also times a raw `pwrite` with no barrier at all, which bounds how much
//! of the cost is the durability barrier versus the surrounding work
//! (`FileFlash::program` reads the target range back first to enforce
//! program-once-per-page, so every program is a read plus a write plus a
//! flush).
//!
//! PLATFORM: file-backed emulation on this host's filesystem. Not NOR flash.
//!
//! `cargo run --release -p slate-kv --example paper_flash_calib`

use slate_kv::file_flash::{Durability, FileFlash};
use slate_kv_hal::Flash;
use std::fs::OpenOptions;
use std::time::Instant;

const CAPACITY: u32 = 4 * 1024 * 1024;
const PAGE: usize = 256;
const BLOCK: usize = 4096;
const N: usize = 300;

fn pct(sorted_ns: &[u64], q: f64) -> f64 {
    let idx = ((q * sorted_ns.len() as f64).ceil() as usize).clamp(1, sorted_ns.len()) - 1;
    sorted_ns[idx] as f64 / 1000.0
}

fn summarize(label: &str, mut ns: Vec<u64>) {
    ns.sort_unstable();
    let n = ns.len();
    let mean = ns.iter().map(|&x| x as u128).sum::<u128>() as f64 / n as f64 / 1000.0;
    println!(
        "{label},{n},{mean:.3},{:.3},{:.3},{:.3},{:.3}",
        pct(&ns, 0.50),
        pct(&ns, 0.90),
        pct(&ns, 0.99),
        ns[n - 1] as f64 / 1000.0
    );
}

fn time_flash_programs(dir: &std::path::Path, label: &str, dur: Durability) {
    let path = dir.join(format!("calib_{label}.bin"));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .unwrap();
    let mut flash = FileFlash::new(file, CAPACITY, PAGE, BLOCK, dur).unwrap();
    let buf = [0xA5u8; PAGE];
    let mut ns = Vec::with_capacity(N);
    // Each page may be programmed once, so every iteration targets a fresh
    // page rather than re-erasing (an erase is a different cost).
    for i in 0..N {
        let addr = (i * PAGE) as u32;
        let t0 = Instant::now();
        flash.program(addr, &buf).unwrap();
        ns.push(t0.elapsed().as_nanos() as u64);
    }
    summarize(&format!("FileFlash::program,{label}"), ns);
    drop(flash);
    let _ = std::fs::remove_file(&path);
}

fn main() {
    let dir = std::env::temp_dir().join(format!("slate_calib_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    println!("# SLATE paper measurement: per-page flash barrier cost, HAL level, no engine");
    println!(
        "# platform=host os={} arch={} backend=FileFlash(file-backed emulation) NOT_real_NOR_flash",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("# geometry: capacity={CAPACITY} page={PAGE} block={BLOCK} n_programs_per_mode={N}");
    println!(
        "# Durability::Full = fcntl(F_FULLFSYNC); Durability::OsCache = std File::sync_data(). \
         raw_pwrite_no_barrier programs the same page size with no flush at all, as a floor."
    );
    println!("operation,mode,n,mean_us,p50_us,p90_us,p99_us,max_us");

    time_flash_programs(&dir, "Full", Durability::Full);
    time_flash_programs(&dir, "OsCache", Durability::OsCache);

    // Barrier primitives in isolation. `Durability::OsCache` calls
    // `File::sync_data()` expecting it to be the cheap barrier, so whether it
    // actually is cheaper than `F_FULLFSYNC` on this platform is measured here
    // rather than assumed from the flag's name.
    {
        use std::os::unix::fs::FileExt;
        use std::os::unix::io::AsRawFd;
        let path = dir.join("calib_barrier.bin");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.set_len(CAPACITY as u64).unwrap();
        let buf = [0x5Au8; PAGE];
        let fd = file.as_raw_fd();

        let mut rust_sync_data = Vec::with_capacity(N);
        let mut libc_fsync = Vec::with_capacity(N);
        let mut libc_fullfsync = Vec::with_capacity(N);
        for i in 0..N {
            let off = ((i % 1024) * PAGE) as u64;
            file.write_all_at(&buf, off).unwrap();
            let t0 = Instant::now();
            file.sync_data().unwrap();
            rust_sync_data.push(t0.elapsed().as_nanos() as u64);

            file.write_all_at(&buf, off).unwrap();
            let t0 = Instant::now();
            let rc = unsafe { libc::fsync(fd) };
            libc_fsync.push(t0.elapsed().as_nanos() as u64);
            assert_eq!(rc, 0);

            file.write_all_at(&buf, off).unwrap();
            let t0 = Instant::now();
            let rc = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
            libc_fullfsync.push(t0.elapsed().as_nanos() as u64);
            assert_ne!(rc, -1);
        }
        summarize("barrier_only,rust_File::sync_data", rust_sync_data);
        summarize("barrier_only,libc_fsync", libc_fsync);
        summarize("barrier_only,libc_fcntl_F_FULLFSYNC", libc_fullfsync);
        drop(file);
        let _ = std::fs::remove_file(&path);
    }

    // Barrier-free floor: same page size, same file API, no flush.
    {
        use std::os::unix::fs::FileExt;
        let path = dir.join("calib_raw.bin");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.set_len(CAPACITY as u64).unwrap();
        let buf = [0xA5u8; PAGE];
        let mut ns = Vec::with_capacity(N);
        for i in 0..N {
            let t0 = Instant::now();
            file.write_all_at(&buf, (i * PAGE) as u64).unwrap();
            ns.push(t0.elapsed().as_nanos() as u64);
        }
        summarize("raw_pwrite,no_barrier", ns);
        drop(file);
        let _ = std::fs::remove_file(&path);
    }

    let _ = std::fs::remove_dir_all(&dir);
}
