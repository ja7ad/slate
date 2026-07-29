//! Paper measurement: put/get throughput and per-operation latency
//! distribution as a function of the commit batch `b_commit`.
//!
//! Extends the sweep in `pi_bench.rs` (which reports ops/s only, for
//! `b_commit ∈ {3,9,27,81}` at `Durability::OsCache`) in four ways, and is a
//! separate example so `pi_bench`'s published meaning does not change:
//!
//!   1. `b_commit ∈ {1,2,4,8,16,27,32,64,128}` — spans `b_min` to `b_max`.
//!   2. Per-operation latency percentiles, not just an aggregate rate.
//!   3. The commit / non-commit latency split. Only every `b_commit`-th put
//!      pays the flash program + durability barrier; reporting one blended
//!      mean hides a bimodal distribution whose two modes differ by orders
//!      of magnitude. A put is classified as the commit-bearing one by
//!      observing `acked_seq()` advance across it, which is what `commit()`
//!      sets — no timing threshold is used to classify.
//!   4. Both `Durability` modes. `Full` is `F_FULLFSYNC` on Darwin (forces
//!      the drive cache to stable media); `OsCache` is `sync_data` (returns
//!      once the OS has the data, drive cache not flushed) and is documented
//!      as benchmark-only. Every number is labelled with the mode that
//!      produced it.
//!
//! PLATFORM HONESTY: this runs against `FileFlash`, a file-backed emulation
//! of a NOR flash chip on whatever filesystem the temp directory lives on. It
//! is NOT Raspberry Pi silicon, NOT an ESP32, and NOT NOR flash. A `program`
//! here is a `pwrite` plus a durability barrier, and an `erase` is a 0xFF
//! overwrite. The absolute numbers characterise this host; only the *shape*
//! of the curve versus `b_commit` transfers to a device.
//!
//! `cargo run --release -p slate-kv --example paper_throughput`

use slate_kv::file_flash::Durability;
use slate_kv::{Db, KeySource, Options, Profile};
use std::time::Instant;

/// 8 MiB: large enough that the whole sweep runs without a segment reclaim,
/// so no row is confounded by GC traffic. Verified by the reported `erases`
/// and `gc_relocated` columns being zero.
const CAPACITY: u32 = 8 * 1024 * 1024;
const VAL_LEN: usize = 100;
const N_DISTINCT: usize = 1000;
const N_PUTS: usize = 2000;
const N_GETS: usize = 2000;
const REPS: usize = 3;
const B_SWEEP: [u32; 9] = [1, 2, 4, 8, 16, 27, 32, 64, 128];

/// Latency summary of one sample, in microseconds.
struct Lat {
    n: usize,
    mean_us: f64,
    p50_us: f64,
    p90_us: f64,
    p99_us: f64,
    max_us: f64,
}

/// Nearest-rank percentile on an ascending sample. Reported rather than an
/// interpolated percentile so every printed value is an observation that
/// actually occurred.
fn nearest_rank_us(sorted_ns: &[u64], q: f64) -> f64 {
    let n = sorted_ns.len();
    let rank = (q * n as f64).ceil() as usize;
    let idx = rank.clamp(1, n) - 1;
    sorted_ns[idx] as f64 / 1000.0
}

/// `None` for an empty sample: at `b_commit = 1` every put commits, so the
/// non-commit sample is genuinely empty and must be reported as absent rather
/// than as a zero.
fn summarize(ns: &mut [u64]) -> Option<Lat> {
    if ns.is_empty() {
        return None;
    }
    ns.sort_unstable();
    let n = ns.len();
    let sum: u128 = ns.iter().map(|&x| x as u128).sum();
    Some(Lat {
        n,
        mean_us: sum as f64 / n as f64 / 1000.0,
        p50_us: nearest_rank_us(ns, 0.50),
        p90_us: nearest_rank_us(ns, 0.90),
        p99_us: nearest_rank_us(ns, 0.99),
        max_us: ns[n - 1] as f64 / 1000.0,
    })
}

/// Six CSV fields: `n,mean,p50,p90,p99,max`. An absent sample emits six empty
/// fields, never placeholder numbers.
fn lat_fields(l: &Option<Lat>) -> String {
    match l {
        Some(x) => format!(
            "{},{:.3},{:.3},{:.3},{:.3},{:.3}",
            x.n, x.mean_us, x.p50_us, x.p90_us, x.p99_us, x.max_us
        ),
        None => ",,,,,".to_string(),
    }
}

fn main() {
    let root = std::env::temp_dir().join(format!("slate_paper_thr_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    // Keys are built before the timed region so that neither the format! nor
    // the allocation lands inside a measured put.
    let keys: Vec<String> = (0..N_DISTINCT).map(|i| format!("sensor_{i:06}")).collect();
    let val = vec![0xA5u8; VAL_LEN];

    println!("# SLATE paper measurement: put/get throughput and latency vs commit batch b_commit");
    println!(
        "# platform=host os={} arch={} backend=FileFlash(file-backed NOR emulation) \
         NOT_raspberry_pi NOT_esp32 NOT_real_NOR_flash",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "# geometry: capacity={CAPACITY} page=256 block=4096 profile=Pi auto_b=false \
         staleness_budget_ms=1000 n_keys=2048 val_len={VAL_LEN} n_distinct_keys={N_DISTINCT} \
         n_puts={N_PUTS} n_gets={N_GETS} reps={REPS}"
    );
    println!(
        "# durability=Full is F_FULLFSYNC on macOS; durability=OsCache is sync_data \
         (benchmark-only). commit-bearing puts identified by acked_seq() advancing, not by timing."
    );
    println!(
        "# stats columns are the post-put-phase snapshot, taken before the explicit flush commit."
    );
    println!(
        "# realized_puts_per_commit = acked_seq/commits, NOT n_puts/commits: the trailing partial \
         batch is never flushed, and dividing by n_puts reports a batch larger than b_commit \
         (133.3 at b_commit=128) purely from that tail. Asserted to within 0.5 of b_commit."
    );
    println!(
        "# CAVEAT, engine bug found while measuring: user_bytes is DOUBLE-COUNTED. \
         Slate::append_hot adds (REC_OVERHEAD+key+val) and Db::put adds the same amount again for \
         the same record, so every user_bytes below -- and any write amplification derived from it \
         -- is exactly 2x too large. parity_bytes and marker_bytes are unaffected. Independently \
         confirmed in slate-kv-sim by the same 2.0 ratio."
    );
    println!(
        "# get latency is served from the mounted volume with the index resident in RAM; the get \
         phase follows an explicit commit, so no key is read out of the in-RAM batch. It is a warm \
         path and includes no mount or replay cost."
    );
    println!(
        "# one row per (durability, b_commit, rep); rep is an independent freshly-formatted volume."
    );
    println!(
        "durability,b_commit,rep,\
         put_ops,put_wall_s,put_ops_per_s,\
         put_all_n,put_all_mean_us,put_all_p50_us,put_all_p90_us,put_all_p99_us,put_all_max_us,\
         put_commit_n,put_commit_mean_us,put_commit_p50_us,put_commit_p90_us,put_commit_p99_us,\
         put_commit_max_us,\
         put_noncommit_n,put_noncommit_mean_us,put_noncommit_p50_us,put_noncommit_p90_us,\
         put_noncommit_p99_us,put_noncommit_max_us,\
         get_ops,get_hits,get_wall_s,get_ops_per_s,\
         get_n,get_mean_us,get_p50_us,get_p90_us,get_p99_us,get_max_us,\
         commits,wakes,realized_puts_per_commit,acked_seq,live_keys,\
         user_bytes,parity_bytes,marker_bytes,gc_bytes,ckpt_bytes,erases,gc_relocated"
    );

    for (dur_label, dur) in [("Full", Durability::Full), ("OsCache", Durability::OsCache)] {
        for &b in &B_SWEEP {
            for rep in 0..REPS {
                let dir = root.join(format!("{dur_label}_b{b}_r{rep}"));
                std::fs::create_dir_all(&dir).unwrap();

                let opts = Options {
                    capacity: CAPACITY,
                    b_commit: b,
                    auto_b: false,
                    staleness_budget_ms: 1000,
                    n_keys: 2048,
                    profile: Profile::Pi,
                    durability: dur,
                };
                let db = Db::open(&dir, KeySource::Bytes([0x42; 32]), opts).unwrap();

                let mut all_ns: Vec<u64> = Vec::with_capacity(N_PUTS);
                let mut commit_ns: Vec<u64> = Vec::new();
                let mut noncommit_ns: Vec<u64> = Vec::with_capacity(N_PUTS);
                let mut prev_acked = db.acked_seq();

                let put_wall_start = Instant::now();
                for i in 0..N_PUTS {
                    let key = keys[i % N_DISTINCT].as_bytes();
                    let t0 = Instant::now();
                    db.put(key, &val).unwrap();
                    let dt = t0.elapsed().as_nanos() as u64;
                    // Classification is read after the timer stops, so the
                    // extra lock does not inflate the measured latency.
                    let acked = db.acked_seq();
                    if acked != prev_acked {
                        prev_acked = acked;
                        commit_ns.push(dt);
                    } else {
                        noncommit_ns.push(dt);
                    }
                    all_ns.push(dt);
                }
                let put_wall = put_wall_start.elapsed().as_secs_f64();

                // Snapshot before flushing, so `commits` counts only the
                // commits the scheduler itself triggered.
                let stats = db.stats();
                let acked_seq = db.acked_seq();
                db.commit().unwrap();

                let mut get_ns: Vec<u64> = Vec::with_capacity(N_GETS);
                let mut hits = 0u64;
                let get_wall_start = Instant::now();
                for i in 0..N_GETS {
                    let key = keys[i % N_DISTINCT].as_bytes();
                    let t0 = Instant::now();
                    let got = db.get(key).unwrap();
                    get_ns.push(t0.elapsed().as_nanos() as u64);
                    if got.is_some() {
                        hits += 1;
                    }
                }
                let get_wall = get_wall_start.elapsed().as_secs_f64();

                let live_keys = db.len();

                let all = summarize(&mut all_ns);
                let com = summarize(&mut commit_ns);
                let non = summarize(&mut noncommit_ns);
                let get = summarize(&mut get_ns);

                // Saturation guard: the realized batch size must track the
                // requested b_commit. If the deadline clamp or an epoch seal
                // forced extra commits this diverges from b_commit and the row
                // is not measuring the operating point it claims to.
                //
                // Normalised by *acked* puts, not N_PUTS. The trailing partial
                // batch is never committed, so N_PUTS/commits credits the last
                // commit with puts it did not flush and reports a realized
                // batch above b_commit (133.3 instead of 128 at b_commit=128)
                // purely from that tail.
                let realized = if stats.commits == 0 {
                    f64::NAN
                } else {
                    acked_seq as f64 / stats.commits as f64
                };
                assert!(
                    (realized - b as f64).abs() < 0.5 || stats.commits == 0,
                    "b_commit={b} but realized batch was {realized:.3}: the deadline clamp or an \
                     epoch seal is forcing commits and this row does not measure the operating \
                     point it claims"
                );

                println!(
                    "{dur_label},{b},{rep},\
                     {N_PUTS},{put_wall:.6},{put_ops_s:.1},\
                     {all_f},{com_f},{non_f},\
                     {N_GETS},{hits},{get_wall:.6},{get_ops_s:.1},{get_f},\
                     {commits},{wakes},{realized:.4},{acked_seq},{live_keys},\
                     {user},{parity},{marker},{gc},{ckpt},{erases},{gc_reloc}",
                    put_ops_s = N_PUTS as f64 / put_wall,
                    all_f = lat_fields(&all),
                    com_f = lat_fields(&com),
                    non_f = lat_fields(&non),
                    get_ops_s = N_GETS as f64 / get_wall,
                    get_f = lat_fields(&get),
                    commits = stats.commits,
                    wakes = stats.wakes,
                    user = stats.user_bytes,
                    parity = stats.parity_bytes,
                    marker = stats.marker_bytes,
                    gc = stats.gc_bytes,
                    ckpt = stats.ckpt_bytes,
                    erases = stats.erases,
                    gc_reloc = stats.gc_relocated,
                );

                drop(db);
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }

    let _ = std::fs::remove_dir_all(&root);
}
