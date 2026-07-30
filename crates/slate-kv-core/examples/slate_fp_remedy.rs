//! Does mixing the key before fingerprinting repair the sequential-key
//! collision spike?
//!
//! `slate_index` establishes that the shipped `index::fingerprint` — the top
//! byte of FNV-1a — is skewed for low-entropy sequential keys, driving the
//! absent-key collision rate to 5.7x the `2b * 2^-f` bound. A natural remedy
//! is to pass the hash through an avalanche finalizer so the top byte depends
//! on every input bit.
//!
//! This probe measures the remedy WITHOUT changing the engine. It reimplements
//! the index's own candidate-selection arithmetic over the public
//! `index::{fingerprint, bucket1, alt_bucket}` surface, once with the shipped
//! fingerprint and once with a finalized one, on identical key populations.
//! Reimplementing is what makes the comparison possible: the shipped
//! `Index` has no hook to substitute the fingerprint function.
//!
//! Writes `docs/porposal/data/fp_remedy.csv`.
//!
//! Run: cargo run --release -p slate-kv-core --example slate_fp_remedy

use slate_kv_core::config::BUCKET_SLOTS;

/// Table sizes swept, matching `slate_index`.
const BUCKETS: [usize; 4] = [256, 1024, 4096, 16384];
const SEEDS: [u64; 3] = [1, 2, 3];
const ABSENT_LOOKUPS: usize = 50_000;

/// FNV-1a, identical to `index::h64` (which is private).
fn h64(key: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in key {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// SplitMix64 finalizer: a bijection, so distinct hashes stay distinct while
/// every output bit depends on every input bit.
fn finalize(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Shipped scheme: fingerprint from the top byte of the raw FNV-1a hash.
fn fp_shipped(key: &[u8]) -> u8 {
    let f = (h64(key) >> 56) as u8;
    if f == 0 {
        1
    } else {
        f
    }
}

/// Proposed scheme: fingerprint from the top byte of the finalized hash.
fn fp_finalized(key: &[u8]) -> u8 {
    let f = (finalize(h64(key)) >> 56) as u8;
    if f == 0 {
        1
    } else {
        f
    }
}

/// The engine's bucket arithmetic, over the public surface.
fn buckets_of(key: &[u8], fp: u8, n: usize) -> (usize, usize) {
    let h = h64(key);
    let i = slate_kv_core::index::bucket1(h, n);
    let j = slate_kv_core::index::alt_bucket(i, fp, n);
    (i, j)
}

/// `tag | seed | ordinal` little-endian, matching `slate_index::make_key`.
/// Sequential ordinals leave the high ordinal bytes zero for every key.
fn make_key(tag: u8, seed: u64, i: usize) -> [u8; 17] {
    let mut k = [0u8; 17];
    k[0] = tag;
    k[1..9].copy_from_slice(&seed.to_le_bytes());
    k[9..17].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

/// Wilson score interval at 95%.
fn wilson95(hits: u64, n: u64) -> (f64, f64) {
    if n == 0 {
        return (0.0, 0.0);
    }
    let z = 1.959_963_984_540_054_f64;
    let nf = n as f64;
    let p = hits as f64 / nf;
    let d = 1.0 + z * z / nf;
    let c = p + z * z / (2.0 * nf);
    let s = z * ((p * (1.0 - p) / nf) + z * z / (4.0 * nf * nf)).sqrt();
    (((c - s) / d).max(0.0), ((c + s) / d).min(1.0))
}

/// Absent-key collision rate for one fingerprint function.
///
/// Fills a table of `(bucket -> fingerprints)` by the same
/// least-loaded-of-two rule the index uses, then counts how many
/// never-inserted keys find a matching fingerprint in either candidate bucket.
fn collision_rate(
    fp_fn: fn(&[u8]) -> u8,
    n_buckets: usize,
    seed: u64,
    n_store: usize,
) -> (u64, u64, usize) {
    let mut table: Vec<Vec<u8>> = vec![Vec::with_capacity(BUCKET_SLOTS); n_buckets];
    let mut fp_hist = [0u64; 256];

    for i in 0..n_store {
        let k = make_key(b'S', seed, i);
        let fp = fp_fn(&k);
        let (a, b) = buckets_of(&k, fp, n_buckets);
        // Least-loaded of the two candidate buckets, as cuckoo insertion does.
        let tgt = if table[a].len() <= table[b].len() {
            a
        } else {
            b
        };
        if table[tgt].len() < BUCKET_SLOTS {
            table[tgt].push(fp);
            fp_hist[fp as usize] += 1;
        }
    }

    let mut hits = 0u64;
    for i in 0..ABSENT_LOOKUPS {
        let k = make_key(b'A', seed, i);
        let fp = fp_fn(&k);
        let (a, b) = buckets_of(&k, fp, n_buckets);
        if table[a].contains(&fp) || table[b].contains(&fp) {
            hits += 1;
        }
    }
    let distinct = fp_hist.iter().filter(|&&c| c > 0).count();
    (hits, ABSENT_LOOKUPS as u64, distinct)
}

fn main() {
    let bound = 2.0 * BUCKET_SLOTS as f64 / 256.0;
    println!("# file: fp_remedy.csv  probe: does mixing before fingerprinting repair the");
    println!("#   sequential-key collision spike reported by slate_index?");
    println!("# command: cargo run --release -p slate-kv-core --example slate_fp_remedy");
    println!("# source: crates/slate-kv-core/examples/slate_fp_remedy.rs");
    println!("# platform=host os=macos arch=aarch64 (pure in-RAM computation, no flash)");
    println!(
        "# scheme=shipped is index::fingerprint (top byte of raw FNV-1a); scheme=finalized \
         passes the hash through a SplitMix64 avalanche finalizer first."
    );
    println!(
        "# Both schemes use the SAME keys (tag|seed|little-endian ordinal, sequential \
         ordinals) and the SAME bucket arithmetic (index::bucket1 / index::alt_bucket), \
         so the fingerprint function is the only difference."
    );
    println!(
        "# NOTE: this reimplements the index's candidate selection over the public \
         index:: surface because Index has no hook to substitute the fingerprint \
         function. It is therefore a MODEL of the remedy, not a measurement of a \
         modified engine. bound 2b*2^-f = {bound:.5}"
    );
    println!(
        "# n_store = floor(0.95 * n_buckets * BUCKET_SLOTS); absent lookups = {ABSENT_LOOKUPS}"
    );
    println!("scheme,n_buckets,seed,n_store,fp_distinct_stored,hits,lookups,rate,ci_lo,ci_hi,bound,over_bound");

    for &n_buckets in &BUCKETS {
        let n_store = (0.95 * (n_buckets * BUCKET_SLOTS) as f64) as usize;
        for &seed in &SEEDS {
            for (name, f) in [
                ("shipped", fp_shipped as fn(&[u8]) -> u8),
                ("finalized", fp_finalized as fn(&[u8]) -> u8),
            ] {
                let (hits, n, distinct) = collision_rate(f, n_buckets, seed, n_store);
                let rate = hits as f64 / n as f64;
                let (lo, hi) = wilson95(hits, n);
                println!(
                    "{name},{n_buckets},{seed},{n_store},{distinct},{hits},{n},\
                     {rate:.6},{lo:.6},{hi:.6},{bound:.5},{:.4}",
                    rate / bound
                );
            }
        }
    }
}
