//! Corrected write-amplification sweep for the proposal paper.
//!
//! Differences from `wa_study.rs` (the original harness):
//!
//! * **Realized utilisation is pinned, not derived.** The original computed
//!   `cap_segs = ceil(n_keys / (u * seg_recs))`, so the realized utilisation was
//!   whatever the ceiling happened to land on (target 0.80 -> realized 0.781,
//!   target 0.90 -> realized 0.893) and was byte-identical across every skew
//!   level, because it was a closed form of `u_target` rather than a
//!   measurement. Here capacity is FIXED (`cap_segs` x `seg_recs`) and the key
//!   count is chosen as `round(u * cap_records)`, so realized utilisation
//!   tracks the target to within one record. The live-key count is still read
//!   back out of the simulator state and reported, so the column is measured.
//! * **Capacity matching.** The original gave the hot/cold arm two EXTRA
//!   segments (`cap_segs += 2`), so it was compared against single-head GC at a
//!   *lower* realized utilisation - a capacity confound, not an age-separation
//!   result. Four arms are emitted here: `single`, `hot_cold` (gross-capacity
//!   matched), `hot_cold_net` (matched on capacity actually reachable by the
//!   allocator, i.e. after the reserve), and `hot_cold_extra2` (reproduces the
//!   original's +2-segment advantage).
//! * **Seeds.** One row per (u, s, gc_type, seed) so the spread is visible.
//! * **Steady state.** WA is reported both over the whole run and over the
//!   second half only, excluding the initial-fill transient (during which
//!   almost no GC happens, deflating WA).
//! * **Bound test at every cell**, including u > 0.8, which the original
//!   skipped.
//!
//! This is a standalone GC model, NOT the SLATE engine: no RS(12,8) parity, no
//! commit markers, no checkpoints, no encryption. See the `RS_PARITY_FACTOR`
//! column for the parity contribution alone.
//!
//! Run: `cargo run --release -p slate-kv-sim --bin wa_study_paper`

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use slate_kv_erasure::{RS_K, RS_M};

/// Bytes written per byte of user data when a segment is sealed, from the
/// erasure-coding geometry alone: `(k + m) / k`.
const RS_PARITY_FACTOR: f64 = (RS_K + RS_M) as f64 / RS_K as f64;

/// Default fixed physical capacity, in segments, for every arm of the sweep.
const CAP_SEGS_DEFAULT: usize = 64;
/// Records per segment.
const SEG_RECS: usize = 64;
/// Default user writes per run, after the initial fill. Override with
/// `SLATE_WA_NOPS` to test steady-state convergence.
const N_OPS_DEFAULT: usize = 100_000;

fn n_ops() -> usize {
    std::env::var("SLATE_WA_NOPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(N_OPS_DEFAULT)
}

/// Segment counts used by the capacity-sensitivity mode. The original harness
/// derived its segment count from the target utilisation
/// (`ceil(n_keys / (u * seg_recs))`), which for n_keys=2000, seg_recs=64 gives 63
/// segments at u=0.5 but only 35 at u=0.9 - so its sweep varied capacity and
/// utilisation together. This mode holds u fixed and varies capacity alone.
const CAP_SWEEP: [usize; 7] = [16, 24, 35, 48, 64, 96, 128];

fn zipf_cdf(n: usize, s: f64) -> Vec<f64> {
    let mut cdf = Vec::with_capacity(n);
    let mut sum = 0.0;
    for i in 1..=n {
        sum += 1.0 / (i as f64).powf(s);
        cdf.push(sum);
    }
    let total = sum;
    for val in &mut cdf {
        *val /= total;
    }
    cdf
}

#[derive(Clone, Copy)]
struct Arm {
    name: &'static str,
    /// Segments added on top of `CAP_SEGS`.
    extra_segs: usize,
    hot_cold: bool,
    /// What the arm is matched on, for the CSV.
    matched: &'static str,
}

const ARMS: [Arm; 4] = [
    Arm {
        name: "single",
        extra_segs: 0,
        hot_cold: false,
        matched: "reference",
    },
    Arm {
        name: "hot_cold",
        extra_segs: 0,
        hot_cold: true,
        matched: "gross",
    },
    Arm {
        name: "hot_cold_net",
        extra_segs: 1,
        hot_cold: true,
        matched: "net",
    },
    Arm {
        name: "hot_cold_extra2",
        extra_segs: 2,
        hot_cold: true,
        matched: "none",
    },
];

/// Raised when the allocator has no free segment to place a record into. With
/// the reserve discipline below this must never happen; it is counted and
/// reported rather than silently absorbed.
struct Starved;

struct Sim {
    cap_segs: usize,
    hot_cold: bool,
    reserve: usize,
    segs: Vec<Vec<usize>>,
    live_ct: Vec<usize>,
    /// `live_seg[key]` = segment currently holding the live version of `key`,
    /// or `usize::MAX` if the key has never been written. Dense because keys are
    /// `0..n_keys`; a HashMap here dominated runtime at high utilisation.
    live_seg: Vec<usize>,
    live_keys: usize,
    free: Vec<usize>,
    hot_head: usize,
    cold_head: usize,
    user: u64,
    copied: u64,
    gc_calls: u64,
    victims: u64,
    guard_hits: u64,
    starves: u64,
    victim_live_sum: f64,
}

impl Sim {
    fn new(cap_segs: usize, hot_cold: bool, n_keys: usize) -> Self {
        let mut free: Vec<usize> = (0..cap_segs).collect();
        let hot_head = free.pop().expect("capacity >= 1");
        let cold_head = if hot_cold {
            free.pop().expect("capacity >= 2")
        } else {
            hot_head
        };
        Self {
            cap_segs,
            hot_cold,
            reserve: if hot_cold { 3 } else { 2 },
            segs: vec![Vec::new(); cap_segs],
            live_ct: vec![0; cap_segs],
            live_seg: vec![usize::MAX; n_keys],
            live_keys: 0,
            free,
            hot_head,
            cold_head,
            user: 0,
            copied: 0,
            gc_calls: 0,
            victims: 0,
            guard_hits: 0,
            starves: 0,
            victim_live_sum: 0.0,
        }
    }

    /// Capacity in records that the allocator can actually fill: the reserve
    /// segments are held free so GC always has somewhere to copy into.
    fn net_records(&self) -> usize {
        (self.cap_segs - self.reserve) * SEG_RECS
    }

    fn place(&mut self, key: usize, is_copy: bool) -> Result<(), Starved> {
        let to_cold = self.hot_cold && is_copy;
        let mut head = if to_cold {
            self.cold_head
        } else {
            self.hot_head
        };
        if self.segs[head].len() >= SEG_RECS {
            match self.free.pop() {
                Some(next) => {
                    head = next;
                    if to_cold {
                        self.cold_head = next;
                    } else {
                        self.hot_head = next;
                    }
                }
                None => {
                    self.starves += 1;
                    return Err(Starved);
                }
            }
        }
        let old = self.live_seg[key];
        if old == usize::MAX {
            self.live_keys += 1;
        } else {
            self.live_ct[old] -= 1;
        }
        self.segs[head].push(key);
        self.live_ct[head] += 1;
        self.live_seg[key] = head;
        if is_copy {
            self.copied += 1;
        } else {
            self.user += 1;
        }
        Ok(())
    }

    fn pick_victim(&self) -> Option<usize> {
        let mut best = None;
        let mut best_live = usize::MAX;
        for s in 0..self.cap_segs {
            if s == self.hot_head || s == self.cold_head || self.segs[s].is_empty() {
                continue;
            }
            if self.live_ct[s] < best_live {
                best_live = self.live_ct[s];
                best = Some(s);
            }
        }
        best
    }

    fn gc(&mut self) {
        self.gc_calls += 1;
        let mut guard = 0;
        while self.free.len() < self.reserve {
            guard += 1;
            if guard > 4 * self.cap_segs {
                self.guard_hits += 1;
                break;
            }
            let Some(victim) = self.pick_victim() else {
                break;
            };
            self.victims += 1;
            self.victim_live_sum += self.live_ct[victim] as f64 / SEG_RECS as f64;
            // Copy the victim's live records out FIRST, then reclaim it. Freeing
            // the victim before the copies (as an earlier draft of this file did)
            // hands the allocator a segment it is still reading from AND makes
            // `place` decrement an already-zeroed `live_ct[victim]`, which wraps
            // in release mode and destroys victim selection.
            let keys = self.segs[victim].clone();
            let mut aborted = false;
            for k in keys {
                if self.live_seg[k] == victim && self.place(k, true).is_err() {
                    aborted = true;
                    break;
                }
            }
            if aborted {
                // Leave the victim intact: its remaining records are still live
                // and still owned by it, so nothing is dropped.
                break;
            }
            debug_assert_eq!(self.live_ct[victim], 0, "victim retained live records");
            self.segs[victim].clear();
            self.live_ct[victim] = 0;
            self.free.push(victim);
        }
    }

    fn write(&mut self, key: usize) {
        let head_full = self.segs[self.hot_head].len() >= SEG_RECS
            || (self.hot_cold && self.segs[self.cold_head].len() >= SEG_RECS);
        if self.free.len() < self.reserve && head_full {
            self.gc();
        }
        if self.place(key, false).is_err() {
            self.gc();
            let _ = self.place(key, false);
        }
    }
}

struct Row {
    cap_ref: usize,
    u_target: f64,
    skew: f64,
    arm: Arm,
    seed: u64,
    n_keys: usize,
    live_keys: usize,
    realized_u: f64,
    realized_u_net: f64,
    wa_all: f64,
    wa_steady: f64,
    victim_live_frac: f64,
    gc_calls: u64,
    victims: u64,
    guard_hits: u64,
    starves: u64,
}

fn run_cell(u_target: f64, skew: f64, arm: Arm, seed: u64, cap_ref: usize) -> Row {
    let cap_segs = cap_ref + arm.extra_segs;
    // Key count is set from the FIXED reference capacity so that every arm
    // holds the same amount of live data; utilisation then differs only by the
    // arm's extra segments, which is exactly the confound under test.
    let n_keys = (u_target * (cap_ref * SEG_RECS) as f64).round() as usize;

    let cdf = zipf_cdf(n_keys, skew);
    let mut rng = StdRng::seed_from_u64(seed);
    let mut sim = Sim::new(cap_segs, arm.hot_cold, n_keys);

    for k in 0..n_keys {
        sim.write(k);
    }
    let fill_user = sim.user;
    let fill_copied = sim.copied;

    let n_ops = n_ops();
    let half = n_ops / 2;
    let mut mid = (0u64, 0u64);
    for i in 0..n_ops {
        if i == half {
            mid = (sim.user, sim.copied);
        }
        let r: f64 = rng.gen();
        let k = cdf.partition_point(|&c| c < r).min(n_keys - 1);
        sim.write(k);
    }

    let post_user = sim.user - fill_user;
    let post_copied = sim.copied - fill_copied;
    let steady_user = sim.user - mid.0;
    let steady_copied = sim.copied - mid.1;

    let cap_records = cap_segs * SEG_RECS;
    Row {
        cap_ref,
        u_target,
        skew,
        arm,
        seed,
        n_keys,
        live_keys: sim.live_keys,
        realized_u: sim.live_keys as f64 / cap_records as f64,
        realized_u_net: sim.live_keys as f64 / sim.net_records() as f64,
        wa_all: (post_user + post_copied) as f64 / post_user as f64,
        wa_steady: (steady_user + steady_copied) as f64 / steady_user as f64,
        victim_live_frac: if sim.victims == 0 {
            f64::NAN
        } else {
            sim.victim_live_sum / sim.victims as f64
        },
        gc_calls: sim.gc_calls,
        victims: sim.victims,
        guard_hits: sim.guard_hits,
        starves: sim.starves,
    }
}

fn emit(r: &Row) {
    let bound = 1.0 / (1.0 - r.realized_u);
    let bound_net = 1.0 / (1.0 - r.realized_u_net);
    let excess = 100.0 * (r.wa_steady - bound) / bound;
    let cap_segs = r.cap_ref + r.arm.extra_segs;
    let reserve = if r.arm.hot_cold { 3 } else { 2 };
    println!(
        "{:.2},{:.1},{},{},{},{},{},{},{},{},{},{:.4},{:.4},{:.4},{:.4},{:.4},\
         {},{:.2},{:.4},{},{:.4},{},{:.4},{},{},{},{}",
        r.u_target,
        r.skew,
        r.arm.name,
        r.seed,
        r.arm.matched,
        r.cap_ref,
        cap_segs,
        cap_segs * SEG_RECS,
        (cap_segs - reserve) * SEG_RECS,
        r.n_keys,
        r.live_keys,
        r.realized_u,
        r.realized_u_net,
        r.wa_all,
        r.wa_steady,
        bound,
        r.wa_steady <= bound,
        excess,
        bound_net,
        r.wa_steady <= bound_net,
        r.wa_steady * RS_PARITY_FACTOR,
        r.starves > 0 || r.guard_hits > 0,
        r.victim_live_frac,
        r.gc_calls,
        r.victims,
        r.guard_hits,
        r.starves,
    );
}

fn main() {
    let capacity_mode = std::env::args().any(|a| a == "--capacity-sweep");

    let us = [0.5, 0.6, 0.7, 0.8, 0.9];
    let skews = [0.0, 0.6, 0.9, 1.2];
    let seeds = [1u64, 2, 3];

    println!(
        "# wa_study_paper{mode}: standalone segment-GC model (NOT the SLATE engine). \
         seg_recs={SEG_RECS} n_ops={nops} \
         rs_parity_factor={RS_PARITY_FACTOR:.4} (RS_K={RS_K},RS_M={RS_M})",
        mode = if capacity_mode {
            " --capacity-sweep"
        } else {
            ""
        },
        nops = n_ops(),
    );
    println!(
        "u_target,s,gc_type,seed,capacity_matched,cap_ref_segs,cap_segs,cap_records,\
         net_records,n_keys,live_keys,realized_u,realized_u_net,wa_all,wa_steady,\
         bound_1_over_1_minus_u,bound_ok,bound_excess_pct,bound_net,bound_net_ok,\
         wa_steady_with_rs_parity,degenerate,victim_live_frac,gc_calls,victims,guard_hits,starves"
    );

    if capacity_mode {
        // Hold utilisation and skew fixed; vary physical capacity alone. This
        // isolates the size effect that the original harness folded into its u
        // axis (its cap_segs fell from 63 at u=0.5 to 35 at u=0.9).
        for &cap in &CAP_SWEEP {
            for &s in &skews {
                for &u in &us {
                    for arm in ARMS {
                        for &seed in &seeds {
                            emit(&run_cell(u, s, arm, seed, cap));
                        }
                    }
                }
            }
        }
        return;
    }

    for &s in &skews {
        for &u in &us {
            for arm in ARMS {
                for &seed in &seeds {
                    emit(&run_cell(u, s, arm, seed, CAP_SEGS_DEFAULT));
                }
            }
        }
    }
}
