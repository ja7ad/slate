//! Paper measurement: partial-key cuckoo index cost.
//!
//! Sweeps the table size and, for each `(n_buckets, seed)` pair, measures on the
//! real `Index` code path:
//!
//! * the load factor reached at the *first* failed insertion (theory: α = 0.95
//!   at `b = 4`),
//! * arena bytes actually consumed per key stored,
//! * slot probes per lookup, against the `2b + s = 16` bound,
//! * the fingerprint-collision rate — the fraction of lookups whose candidate
//!   set contains an offset belonging to some other key — against the
//!   `2b · 2^-f = 8/256 = 0.03125` bound, with a Wilson 95 % interval,
//! * stash occupancy at the α = 0.95 operating point and at first failure.
//!
//! Every key is distinct, and key `i` is stored at the unique offset `i + 1`, so
//! "this candidate offset does not belong to the queried key" is decidable
//! exactly, with no flash access and no reliance on the surrounding engine.
//!
//! `cargo run --release -p slate-kv-core --example paper_index`
//!
//! Pure computation: no flash, no clock, no I/O — the numbers are independent of
//! the host machine except for the arena byte counts, which are `u32`-slot
//! arithmetic and therefore identical on the 32-bit targets.

use slate_kv_core::config::{BUCKET_SLOTS, MAX_INDEX_SLOTS, N_BUCKETS, STASH_SIZE};
use slate_kv_core::index::{CandidateBuf, Index, XorShift64};

/// Table sizes to sweep, in buckets. `N_BUCKETS` (2048) is the compile-time
/// default; 16384 buckets is 65 536 slots, exactly `MAX_INDEX_SLOTS`, the
/// largest table the checkpoint format can serialize.
const BUCKET_SWEEP: [usize; 7] = [256, 512, 1024, N_BUCKETS, 4096, 8192, 16384];
const SEEDS: [u64; 8] = [1, 2, 3, 5, 8, 13, 21, 34];
/// Absent-key lookups per (n_buckets, seed) for the negative false-positive rate.
const ABSENT_LOOKUPS: usize = 200_000;

/// Key population shape. The index derives its fingerprint as the *top byte* of
/// FNV-1a, and FNV-1a mixes into its high bits only through the multiply chain,
/// so how much entropy the last-processed key bytes carry decides whether that
/// byte is uniform. Both shapes below are realistic — device keys really are
/// sequential — so both are measured rather than one being chosen.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyFamily {
    /// `tag ‖ seed ‖ ordinal` little-endian: the high ordinal bytes are zero for
    /// every key, exactly like an application using `sensor_%06d` style keys.
    Sequential,
    /// The same ordinals passed through a 64-bit mixing function first, so every
    /// key byte varies. This is the population the `2b · 2^-f` bound assumes.
    Mixed,
}

impl KeyFamily {
    fn name(self) -> &'static str {
        match self {
            KeyFamily::Sequential => "sequential",
            KeyFamily::Mixed => "mixed",
        }
    }
}

/// SplitMix64 finalizer: a bijection on u64, so distinct ordinals stay distinct.
fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Distinct 17-byte key: a namespace tag plus (seed, ordinal).
fn make_key(fam: KeyFamily, tag: u8, seed: u64, i: usize) -> [u8; 17] {
    let mut k = [0u8; 17];
    k[0] = tag;
    k[1..9].copy_from_slice(&seed.to_le_bytes());
    let ord = match fam {
        KeyFamily::Sequential => i as u64,
        KeyFamily::Mixed => splitmix(i as u64 ^ (seed << 32)),
    };
    k[9..17].copy_from_slice(&ord.to_le_bytes());
    k
}

/// Wilson score interval at 95 % for `hits` successes in `n` trials.
fn wilson95(hits: u64, n: u64) -> (f64, f64) {
    if n == 0 {
        return (f64::NAN, f64::NAN);
    }
    let z = 1.959_963_984_540_054_f64;
    let n = n as f64;
    let p = hits as f64 / n;
    let denom = 1.0 + z * z / n;
    let centre = p + z * z / (2.0 * n);
    let margin = z * ((p * (1.0 - p) / n) + (z * z / (4.0 * n * n))).sqrt();
    ((centre - margin) / denom, (centre + margin) / denom)
}

/// Fills a fresh table until `upsert` first returns an error. Returns
/// (keys stored, stash occupancy).
fn fill_to_failure(fam: KeyFamily, n_buckets: usize, seed: u64) -> (usize, usize) {
    let mut slots = vec![0u32; n_buckets * BUCKET_SLOTS];
    let mut index = Index::new(&mut slots, n_buckets);
    let mut rng = XorShift64::new(seed);
    // Hard ceiling: every slot plus the stash plus slack. Reaching it would mean
    // insertion never failed, which the caller must not silently read as a load
    // factor of 1.
    let ceiling = n_buckets * BUCKET_SLOTS + STASH_SIZE + 1;
    let mut i = 0usize;
    while i < ceiling {
        let k = make_key(fam, b'S', seed, i);
        if index
            .upsert(&k, (i + 1) as u32, &mut rng, |_| false)
            .is_err()
        {
            break;
        }
        i += 1;
    }
    assert!(
        i < ceiling,
        "n_buckets={n_buckets} seed={seed}: insertion never failed"
    );
    (index.len(), index.stash_occupancy())
}

struct LoadPoint {
    keys: usize,
    all_inserted: bool,
    stash: usize,
    probes_total: u64,
    probes_max: usize,
    cand_total: u64,
    cand_max: usize,
    fp_hits_present: u64,
    fp_extra_present: u64,
    fp_hits_absent: u64,
    fp_extra_absent: u64,
    occupied_total: u64,
    /// Distinct fingerprint values among the stored keys (max 255).
    fp_distinct_stored: usize,
    /// Distinct fingerprint values among the absent query keys (max 255).
    fp_distinct_absent: usize,
    /// Expected collision rate if fingerprints were uniform over 255 values,
    /// weighted by the *actual* stored-fingerprint histogram: for a query drawn
    /// from the absent population, the chance a given probed occupied slot
    /// matches is sum_f p_absent(f) * p_stored(f), not 1/256.
    fp_match_prob: f64,
}

/// Fills to `target` keys, then measures lookups at that load.
fn measure_at_load(fam: KeyFamily, n_buckets: usize, seed: u64, target: usize) -> LoadPoint {
    let mut slots = vec![0u32; n_buckets * BUCKET_SLOTS];
    let mut index = Index::new(&mut slots, n_buckets);
    let mut rng = XorShift64::new(seed);

    let mut stored = 0usize;
    let mut all_inserted = true;
    // Fingerprint histograms: over the stored population and over the absent
    // query population. Index 0 is never produced (`fingerprint` maps 0 -> 1).
    let mut fp_stored = [0u64; 256];
    let mut fp_absent = [0u64; 256];

    for i in 0..target {
        let k = make_key(fam, b'S', seed, i);
        if index
            .upsert(&k, (i + 1) as u32, &mut rng, |_| false)
            .is_err()
        {
            all_inserted = false;
            break;
        }
        fp_stored[slate_kv_core::index::fingerprint(&k) as usize] += 1;
        stored = i + 1;
    }

    let mut lp = LoadPoint {
        keys: index.len(),
        all_inserted,
        stash: index.stash_occupancy(),
        probes_total: 0,
        probes_max: 0,
        cand_total: 0,
        cand_max: 0,
        fp_hits_present: 0,
        fp_extra_present: 0,
        fp_hits_absent: 0,
        fp_extra_absent: 0,
        occupied_total: 0,
        fp_distinct_stored: 0,
        fp_distinct_absent: 0,
        fp_match_prob: 0.0,
    };

    // Positive lookups: every stored key. A candidate offset other than the
    // key's own `i + 1` is a fingerprint collision.
    for i in 0..stored {
        let k = make_key(fam, b'S', seed, i);
        let mut cbuf = CandidateBuf::new();
        let probes = index.candidates_probed(&k, &mut cbuf);
        let cands = cbuf.as_slice();
        let own = (i + 1) as u32;
        assert!(
            cands.contains(&own),
            "n_buckets={n_buckets} seed={seed} key {i}: own offset missing from candidates"
        );
        let extra = cands.iter().filter(|&&o| o != own).count();
        lp.probes_total += probes as u64;
        lp.probes_max = lp.probes_max.max(probes);
        lp.cand_total += cands.len() as u64;
        lp.cand_max = lp.cand_max.max(cands.len());
        lp.fp_extra_present += extra as u64;
        if extra > 0 {
            lp.fp_hits_present += 1;
        }
    }

    // Negative lookups: keys in a disjoint namespace, never inserted. Any
    // candidate at all is a false positive.
    for i in 0..ABSENT_LOOKUPS {
        let k = make_key(fam, b'A', seed, i);
        fp_absent[slate_kv_core::index::fingerprint(&k) as usize] += 1;
        let mut cbuf = CandidateBuf::new();
        let probes = index.candidates_probed(&k, &mut cbuf);
        let n_cand = cbuf.as_slice().len();
        lp.probes_total += probes as u64;
        lp.probes_max = lp.probes_max.max(probes);
        lp.fp_extra_absent += n_cand as u64;
        if n_cand > 0 {
            lp.fp_hits_absent += 1;
        }
        // Occupied slots examined by this same lookup. `2b * 2^-f` counts 2b
        // chances; the honest denominator is how many of those slots actually
        // held a fingerprint.
        let (_, occ) = index.probe_occupancy(&k);
        lp.occupied_total += occ as u64;
    }

    lp.fp_distinct_stored = fp_stored.iter().filter(|&&c| c > 0).count();
    lp.fp_distinct_absent = fp_absent.iter().filter(|&&c| c > 0).count();
    // Collision probability per probed occupied slot, using the measured
    // fingerprint distributions rather than assuming uniformity. If both are
    // uniform over 255 values this is 1/255; a skewed population makes it larger,
    // which is exactly how the measured rate can exceed the 2^-f bound.
    let n_stored = stored.max(1) as f64;
    let n_absent = ABSENT_LOOKUPS as f64;
    lp.fp_match_prob = (1..256)
        .map(|f| (fp_absent[f] as f64 / n_absent) * (fp_stored[f] as f64 / n_stored))
        .sum();
    lp
}

fn main() {
    println!("# SLATE paper measurement: partial-key cuckoo index cost");
    println!(
        "# cmd=cargo run --release -p slate-kv-core --example paper_index \
         BUCKET_SLOTS={BUCKET_SLOTS} STASH_SIZE={STASH_SIZE} FP_BITS=8 OFF_BITS=24 \
         MAX_KICKS=500 default_N_BUCKETS={N_BUCKETS} MAX_INDEX_SLOTS={MAX_INDEX_SLOTS} \
         key_len=17B absent_lookups_per_row={ABSENT_LOOKUPS} \
         key_families=sequential,mixed seeds={} \
         platform=macOS-26.5.2/arm64 (pure in-RAM computation, no flash)",
        SEEDS.len()
    );
    println!(
        "# alpha095_target = floor(0.95 * n_buckets * BUCKET_SLOTS); \
         fp_rate_* are per-lookup fractions with Wilson 95% intervals; \
         bound 2b*2^-f = {:.5}. \
         key_family=sequential has near-zero entropy in the high ordinal bytes, so its \
         fingerprints are skewed and its collision rate is NOT comparable to the uniform \
         bound; key_family=mixed is the uniform population the bound assumes. \
         fp_bound_measured_distribution recomputes the bound from the measured \
         fingerprint histograms and realized slot occupancy.",
        2.0 * BUCKET_SLOTS as f64 / 256.0
    );
    println!(
        "key_family,n_buckets,n_slots,arena_bytes,stash_ram_bytes,index_ram_bytes,seed,\
         table_capacity,keys_at_first_failure,alpha_at_failure,stash_occ_at_failure,\
         arena_bytes_per_key_at_failure,index_bytes_per_key_at_failure,\
         alpha095_target_keys,alpha095_all_inserted,keys_at_alpha095,stash_occ_at_alpha095,\
         arena_bytes_per_key_at_alpha095,lookups_total,probes_mean,probes_max,probes_bound,\
         cand_mean_present,cand_max_present,\
         fp_lookups_present,fp_hits_present,fp_rate_present,fp_ci_lo_present,fp_ci_hi_present,\
         fp_extra_per_lookup_present,\
         fp_lookups_absent,fp_hits_absent,fp_rate_absent,fp_ci_lo_absent,fp_ci_hi_absent,\
         fp_extra_per_lookup_absent,\
         occupied_slots_per_absent_lookup,fp_bound_2b,fp_bound_realized_occupancy,\
         fp_distinct_stored,fp_distinct_absent,fp_match_prob_measured,\
         fp_bound_measured_distribution"
    );

    let probes_bound = 2 * BUCKET_SLOTS + STASH_SIZE;

    for &fam in &[KeyFamily::Sequential, KeyFamily::Mixed] {
        for &nb in &BUCKET_SWEEP {
            let n_slots = nb * BUCKET_SLOTS;
            let arena_bytes = n_slots * 4;
            // In-RAM stash: [(u8, u32); STASH_SIZE], 4-byte aligned.
            let stash_ram = STASH_SIZE * core::mem::size_of::<(u8, u32)>();
            let index_ram = arena_bytes + core::mem::size_of::<Index<'_>>();
            let cap = n_slots;
            let target = (cap * 95) / 100;

            for &seed in &SEEDS {
                let (fail_keys, fail_stash) = fill_to_failure(fam, nb, seed);
                let lp = measure_at_load(fam, nb, seed, target);

                let alpha_fail = fail_keys as f64 / cap as f64;
                let bpk_arena_fail = arena_bytes as f64 / fail_keys as f64;
                let bpk_index_fail = index_ram as f64 / fail_keys as f64;
                let bpk_arena_095 = arena_bytes as f64 / lp.keys as f64;
                let n_lookups = lp.keys as u64 + ABSENT_LOOKUPS as u64;
                let probes_mean = lp.probes_total as f64 / n_lookups as f64;
                let cand_mean = lp.cand_total as f64 / lp.keys as f64;
                let (plo, phi) = wilson95(lp.fp_hits_present, lp.keys as u64);
                let (alo, ahi) = wilson95(lp.fp_hits_absent, ABSENT_LOOKUPS as u64);
                let occ_per_lookup = lp.occupied_total as f64 / ABSENT_LOOKUPS as f64;
                // The textbook bound: 2b chances at 2^-f each.
                let fp_bound_2b = 2.0 * BUCKET_SLOTS as f64 / 256.0;
                // The same bound with the *measured* number of occupied slots in
                // place of the assumed 2b, i.e. 1 - (1 - 2^-f)^occupied.
                let fp_bound_occ = 1.0 - (1.0 - 1.0 / 256.0f64).powf(occ_per_lookup);
                // Same expression again, but with the measured per-slot match
                // probability in place of the assumed 2^-f. This is the bound the
                // measured rate should actually be compared against.
                let fp_bound_dist = 1.0 - (1.0 - lp.fp_match_prob).powf(occ_per_lookup);

                println!(
                    "{},{nb},{n_slots},{arena_bytes},{stash_ram},{index_ram},{seed},\
                 {cap},{fail_keys},{alpha_fail:.6},{fail_stash},\
                 {bpk_arena_fail:.4},{bpk_index_fail:.4},\
                 {target},{},{},{},{bpk_arena_095:.4},{n_lookups},{probes_mean:.4},{},\
                 {probes_bound},{cand_mean:.6},{},\
                 {},{},{:.8},{plo:.8},{phi:.8},{:.6},\
                 {ABSENT_LOOKUPS},{},{:.8},{alo:.8},{ahi:.8},{:.6},\
                 {occ_per_lookup:.6},{fp_bound_2b:.8},{fp_bound_occ:.8},\
                 {},{},{:.8},{fp_bound_dist:.8}",
                    fam.name(),
                    u8::from(lp.all_inserted),
                    lp.keys,
                    lp.stash,
                    lp.probes_max,
                    lp.cand_max,
                    lp.keys,
                    lp.fp_hits_present,
                    lp.fp_hits_present as f64 / lp.keys as f64,
                    lp.fp_extra_present as f64 / lp.keys as f64,
                    lp.fp_hits_absent,
                    lp.fp_hits_absent as f64 / ABSENT_LOOKUPS as f64,
                    lp.fp_extra_absent as f64 / ABSENT_LOOKUPS as f64,
                    lp.fp_distinct_stored,
                    lp.fp_distinct_absent,
                    lp.fp_match_prob,
                );
            }
        }
    }
}
