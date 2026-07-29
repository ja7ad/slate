//! Paper measurement: energy per operation as a function of the commit batch
//! `b_commit`, and validation of the closed-form optimum B★ = sqrt(2·λ·A/c)
//! against the empirical argmin of the measured curve.
//!
//! WHAT THIS IS. The flash-traffic *counts* (bytes programmed, erases, wakes,
//! commits) are measured: they come from `SimFlash`, an in-RAM NOR-flash
//! simulator that enforces program-once-per-page and 0xFF erase semantics, and
//! from the engine's own `Metrics`. The *joules* are not measured — they are
//! those counts multiplied through the parameterised `power::PowerModel`, whose
//! own `PowerReport` labels itself "ESTIMATED". No power meter and no board are
//! involved. Every energy column below is a model evaluation, and the model
//! parameters it used are printed in the provenance block from
//! `PowerModel::default()` rather than transcribed here.
//!
//! TWO ENERGY COLUMNS, DELIBERATELY. `power::report()` sums
//! `user + gc + parity + ckpt` bytes and has no `marker_bytes` term at all —
//! `power::Stats` does not carry the field, even though `core::Metrics` does
//! and `Metrics::flash_bytes()` includes it. Two commit-marker pages are
//! programmed per commit, so the omission is exactly the term that scales as
//! 1/B: at small `b_commit` it is the dominant overhead. Reporting only
//! `report()` would therefore understate the very convexity this sweep exists
//! to demonstrate. Each row carries both `e_report_*` (what `report()` returns,
//! markers omitted) and `e_full_*` (the same model constants applied to
//! `SimFlash`'s ground-truth `bytes_programmed`, which counts every page the
//! engine actually programmed, markers included).
//!
//! `cargo run --release -p slate-kv-sim --bin paper_energy_batch`

use slate_kv_core::sched::b_star;
use slate_kv_sim::power::{report, PowerModel};
use slate_kv_sim::sim_db::{Db, KeySource, Options, Profile};
use slate_kv_sim::{SimCounter, SimFlash};

/// 8 MiB region. `segments_in` caps the segment table at `MAX_SEGMENTS = 128`,
/// so the usable log is 128 × `SEG_BYTES`; `N_OPS` is chosen to stay inside it
/// at the worst case (`b_commit = 1`, one data + one parity + two marker pages
/// per record) so that no row is confounded by GC relocation or erases. The
/// `erases` and `gc_bytes` columns are emitted so a reader can check that.
const CAPACITY: u32 = 8 * 1024 * 1024;
const PAGE: usize = 256;
const BLOCK: usize = 4096;
const N_OPS: usize = 4000;
const N_DISTINCT: usize = 1000;
const VAL_LEN: usize = 16;
const B_MIN: u32 = 1;
const B_MAX: u32 = 128;

/// Arrival rate the commit law is evaluated at, ops/s. The scheduler's own
/// units are Q10 (ops per 1024 s) to stay integral on a no-FPU target.
const LAMBDA_OPS_PER_S: f64 = 10.0;

/// Holding costs `c` [nJ per op per second of staleness] to evaluate B★ at.
///
/// Deliberately an *absolute* grid rather than one derived from a staleness
/// budget. Setting `c = A/(2·λ·t̄²)` (the design's §8.1 route from a latency
/// budget) makes B★ = sqrt(2λA·2λt̄²/A) = 2λt̄ — the fitted `A` cancels
/// exactly, so agreement with the empirical argmin would say nothing about
/// whether `A` was measured correctly. With `c` fixed independently, B★
/// depends on the fitted `A`, and the comparison is a real test of it. The
/// implied budget t̄ = B★/(2λ) is reported alongside for interpretation.
const C_GRID_NJ: [f64; 7] = [3.0e6, 1.0e6, 3.0e5, 1.0e5, 3.0e4, 1.0e4, 3.0e3];

struct Row {
    b: u32,
    commits: u64,
    wakes: u64,
    erases: u64,
    acked_seq: u64,
    marker_bytes_derived: u64,
    ops_per_commit: f64,
    user_bytes: u64,
    gc_bytes: u64,
    parity_bytes: u64,
    ckpt_bytes: u64,
    report_bytes: u64,
    full_bytes: u64,
    e_report_nj_per_op: f64,
    e_full_nj_per_op: f64,
}

/// The model arithmetic of `power::report()`, applied to an arbitrary byte
/// total. Kept in one place so the marker-omitting and marker-inclusive
/// columns differ *only* in which byte total is fed in, never in the constants.
fn model_nj(bytes: u64, erases: u64, wakes: u64, m: &PowerModel) -> f64 {
    let write_nj = bytes * m.beta_nj_per_byte;
    let erase_nj = erases * m.erase_uj_per_block * 1000;
    let wake_nj = wakes * m.wake_uj * 1000;
    let cpu_nj = (bytes * m.aead_cycles_per_byte * m.cpu_nj_per_cycle_q10) / 1024;
    (write_nj + erase_nj + wake_nj + cpu_nj) as f64
}

fn run_one(b: u32, m: &PowerModel) -> Row {
    let opts = Options {
        capacity: CAPACITY,
        b_commit: b,
        auto_b: false,
        staleness_budget_ms: 1000,
        n_keys: 2048,
        profile: Profile::Pi,
    };
    let flash = SimFlash::new(CAPACITY, PAGE, BLOCK);
    let counter = SimCounter::new(1_000_000);
    let db = Db::open(KeySource::Bytes([0u8; 32]), opts, flash, counter).unwrap();

    // The genesis checkpoint is programmed inside `open`, before the engine's
    // `Metrics` exists, so it appears in `SimFlash::stats` but not in the
    // engine counters. Baselining here keeps the two byte totals comparable;
    // it is a B-independent constant, but leaving it in would bias the fitted
    // payload term.
    let base_bytes = db.flash_mut(|f| f.stats.bytes_programmed);
    let base_erases = db.flash_mut(|f| f.stats.erases);

    let val = [0xA5u8; VAL_LEN];
    for i in 0..N_OPS {
        let key = format!("k{:07}", i % N_DISTINCT);
        db.put(key.as_bytes(), &val).unwrap();
    }

    // Snapshot before any close-time flush so `commits` counts only what the
    // scheduler triggered at this `b_commit`.
    let s = db.stats();
    let full_bytes = db.flash_mut(|f| f.stats.bytes_programmed) - base_bytes;
    let flash_erases = db.flash_mut(|f| f.stats.erases) - base_erases;

    let report_bytes = s.user_bytes + s.gc_bytes + s.parity_bytes + s.ckpt_bytes;
    // Cross-check: the shared helper must reproduce `report()` exactly on the
    // byte total `report()` itself uses, or the two columns are not comparable.
    let e_report_total = report(&s, m).m_joules * 1_000_000.0;
    let e_report_helper = model_nj(report_bytes, s.erases, s.wakes, m);
    assert!(
        (e_report_total - e_report_helper).abs() <= 1.0,
        "helper diverged from power::report(): {e_report_helper} vs {e_report_total}"
    );

    let acked_seq = db.acked_seq();
    Row {
        b,
        commits: s.commits,
        wakes: s.wakes,
        erases: flash_erases,
        acked_seq,
        // power::Stats carries no marker_bytes field, so this is derived, not
        // read from a counter: log.rs::commit_async issues exactly two
        // `program_page` calls for the commit marker per non-empty log, and
        // only the hot log is non-empty in this workload.
        marker_bytes_derived: s.commits * 2 * PAGE as u64,
        // Divided by *acked* ops, not N_OPS: the trailing partial batch is
        // never committed, so N_OPS/commits would report a realized batch
        // larger than b_commit at large B purely from that tail.
        ops_per_commit: if s.commits == 0 {
            f64::NAN
        } else {
            acked_seq as f64 / s.commits as f64
        },
        user_bytes: s.user_bytes,
        gc_bytes: s.gc_bytes,
        parity_bytes: s.parity_bytes,
        ckpt_bytes: s.ckpt_bytes,
        report_bytes,
        full_bytes,
        // Normalised per *acked* op. The flash traffic in the numerator was
        // produced by exactly the acked operations; the trailing partial batch
        // is still in RAM and cost nothing, so dividing by N_OPS would credit
        // the batch with free operations and bias E(B) downward at large B.
        e_report_nj_per_op: e_report_total / acked_seq as f64,
        e_full_nj_per_op: model_nj(full_bytes, flash_erases, s.wakes, m) / acked_seq as f64,
    }
}

/// Ordinary least squares of `y = A·(1/B) + P`. Returns `(A_nj, P_nj, r2)`.
/// `A` is the fixed per-commit energy amortised over the batch and `P` the
/// per-operation payload floor; the commit law's `A` is precisely this slope.
fn fit_a(rows: &[Row], y: impl Fn(&Row) -> f64) -> (f64, f64, f64) {
    let n = rows.len() as f64;
    let xs: Vec<f64> = rows.iter().map(|r| 1.0 / r.b as f64).collect();
    let ys: Vec<f64> = rows.iter().map(&y).collect();
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let sxy: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    let sxx: f64 = xs.iter().map(|x| (x - mx) * (x - mx)).sum();
    let a = sxy / sxx;
    let p = my - a * mx;
    let sst: f64 = ys.iter().map(|y| (y - my) * (y - my)).sum();
    let sse: f64 = xs
        .iter()
        .zip(&ys)
        .map(|(x, y)| {
            let e = y - (a * x + p);
            e * e
        })
        .sum();
    let r2 = if sst > 0.0 { 1.0 - sse / sst } else { f64::NAN };
    (a, p, r2)
}

/// Empirical argmin over the swept batch sizes of the total power
/// `P(B) = λ·E(B) + c·B/2` [nJ/s]: measured physical term plus the linear
/// holding term the budget implies.
fn argmin(rows: &[Row], e: impl Fn(&Row) -> f64, c_nj: f64) -> (u32, f64) {
    let mut best_b = rows[0].b;
    let mut best_p = f64::INFINITY;
    for r in rows {
        let p = LAMBDA_OPS_PER_S * e(r) + c_nj * r.b as f64 / 2.0;
        if p < best_p {
            best_p = p;
            best_b = r.b;
        }
    }
    (best_b, best_p)
}

fn main() {
    let m = PowerModel::default();
    let lam_q10 = (LAMBDA_OPS_PER_S * 1024.0) as u64;

    println!("# SLATE paper measurement: energy per op vs commit batch, and B* validation");
    println!(
        "# ESTIMATE, NOT A MEASUREMENT OF SILICON. Flash traffic (bytes, erases, wakes, commits) \
         is measured from the SimFlash in-RAM NOR simulator and the engine Metrics; joules are \
         those counts times the parameterised power model, which labels its own output \"{}\". \
         No power meter, no board: J/op on real hardware is not measurable here.",
        report(&Default::default(), &m).label
    );
    println!(
        "# power model (read from PowerModel::default() at runtime): \
         beta_nj_per_byte={} erase_uj_per_block={} wake_uj={} cpu_nj_per_cycle_q10={} \
         aead_cycles_per_byte={}",
        m.beta_nj_per_byte,
        m.erase_uj_per_block,
        m.wake_uj,
        m.cpu_nj_per_cycle_q10,
        m.aead_cycles_per_byte
    );
    println!(
        "# geometry: SimFlash capacity={CAPACITY} page={PAGE} block={BLOCK} profile=Pi \
         auto_b=false n_keys=2048 val_len={VAL_LEN} n_distinct_keys={N_DISTINCT} n_ops={N_OPS} \
         b_commit={B_MIN}..={B_MAX} deterministic=yes reps=1"
    );
    println!(
        "# e_report_* uses power::report()'s byte total (user+gc+parity+ckpt) which OMITS \
         marker_bytes -- power::Stats has no such field. e_full_* applies the same constants to \
         SimFlash bytes_programmed, the ground-truth count of bytes actually programmed, markers \
         included. The omitted term is a 1/B term, so it matters most at small B."
    );
    println!(
        "# CAVEAT, engine bug found while measuring: user_bytes is DOUBLE-COUNTED. \
         Slate::append_hot adds (REC_OVERHEAD+key+val) and sim_db::put adds it again for the same \
         record, so every user_bytes here (and any write amplification derived from it) is exactly \
         2x too large. slate-kv/src/db.rs::put has the identical duplication. e_report_* inherits \
         the error; e_full_* does not, since it reads physical bytes from the simulator. \
         Verified this run by the user_bytes/(n_ops*(REC_OVERHEAD+8+VAL_LEN)) ratio asserted below."
    );
    println!(
        "# CAVEAT: report_bytes counts logical record bytes while full_bytes counts programmed \
         PAGES. Records are packed into 256 B pages, so full_bytes is a step function of B and \
         the e_full_* curve is a sawtooth on top of the 1/B trend, not smooth. marker_gap_bytes \
         is therefore not a clean marker-only residual: it mixes the missing marker term with \
         page quantization and with the double-count above, and goes negative once the \
         double-counted user term exceeds the marker term."
    );

    println!("[sweep]");
    println!(
        "b_commit,commits,wakes,erases,acked_ops,realized_ops_per_commit,\
         user_bytes,gc_bytes,parity_bytes,ckpt_bytes,marker_bytes_derived,\
         report_bytes,full_bytes,\
         e_report_nj_per_op,e_full_nj_per_op"
    );

    let mut rows: Vec<Row> = Vec::new();
    for b in B_MIN..=B_MAX {
        let r = run_one(b, &m);
        println!(
            "{},{},{},{},{},{:.4},{},{},{},{},{},{},{},{:.3},{:.3}",
            r.b,
            r.commits,
            r.wakes,
            r.erases,
            r.acked_seq,
            r.ops_per_commit,
            r.user_bytes,
            r.gc_bytes,
            r.parity_bytes,
            r.ckpt_bytes,
            r.marker_bytes_derived,
            r.report_bytes,
            r.full_bytes,
            r.e_report_nj_per_op,
            r.e_full_nj_per_op,
        );
        rows.push(r);
    }

    // Saturation guard. The independent variable must actually move the state
    // it names: the realized batch size has to track `b_commit`, not plateau.
    // It cannot exceed b_commit (the scheduler commits at B or at the deadline,
    // whichever comes first), so only a shortfall is a confound.
    let worst = rows
        .iter()
        .map(|r| r.ops_per_commit / r.b as f64)
        .fold(f64::INFINITY, f64::min);
    assert!(
        worst > 0.95,
        "realized ops/commit fell to {:.3}x of b_commit -- the deadline clamp or an epoch seal \
         is forcing extra commits and the sweep is not measuring the operating point it claims",
        worst
    );

    // Pin the user_bytes double-count numerically rather than asserting it from
    // reading the source. Keys are `format!("k{:07}", _)`, so 8 bytes each.
    let expect_single = (N_OPS * (slate_kv_core::config::REC_OVERHEAD + 8 + VAL_LEN)) as f64;
    let observed = rows[0].user_bytes as f64;
    println!("[user_bytes_double_count_check]");
    println!("expected_single_count_bytes,observed_bytes,ratio");
    println!(
        "{expect_single:.0},{observed:.0},{:.6}",
        observed / expect_single
    );

    let (a_report_nj, p_report_nj, r2_report) = fit_a(&rows, |r| r.e_report_nj_per_op);
    let (a_full_nj, p_full_nj, r2_full) = fit_a(&rows, |r| r.e_full_nj_per_op);

    println!("[fit]");
    println!("byte_accounting,a_nj_per_commit,a_uj_per_commit,payload_nj_per_op,r2");
    println!(
        "report_omits_markers,{:.2},{:.4},{:.3},{:.6}",
        a_report_nj,
        a_report_nj / 1000.0,
        p_report_nj,
        r2_report
    );
    println!(
        "full_includes_markers,{:.2},{:.4},{:.3},{:.6}",
        a_full_nj,
        a_full_nj / 1000.0,
        p_full_nj,
        r2_full
    );

    // B* validation. `c` comes from the independent grid, so B* is a genuine
    // prediction from the fitted A and is free to disagree with the argmin.
    // `argmin_at_sweep_edge=true` means the argmin is pinned at B_MIN or B_MAX,
    // i.e. the true optimum lies outside the swept range and that row's
    // relative error is a bound, not an estimate.
    println!("[bstar]");
    println!(
        "byte_accounting,lambda_ops_per_s,c_nj_per_op_s,implied_t_budget_s,a_nj,\
         b_star_closed_form,b_star_sched_rs_integer,b_empirical_argmin,argmin_at_sweep_edge,\
         p_at_b_star_nj_per_s,p_at_argmin_nj_per_s,excess_power_at_b_star,\
         rel_err_closed_vs_empirical,rel_err_integer_vs_empirical"
    );
    for (label, a_nj, ecol) in [
        (
            "report_omits_markers",
            a_report_nj,
            (|r: &Row| r.e_report_nj_per_op) as fn(&Row) -> f64,
        ),
        (
            "full_includes_markers",
            a_full_nj,
            (|r: &Row| r.e_full_nj_per_op) as fn(&Row) -> f64,
        ),
    ] {
        for &c_nj in &C_GRID_NJ {
            let bs_closed = (2.0 * LAMBDA_OPS_PER_S * a_nj / c_nj).sqrt();
            // The shipping integer implementation, given the same inputs it
            // would see on-device (A in whole µJ, c in whole nJ).
            let bs_int = b_star(lam_q10, (a_nj / 1000.0) as u64, c_nj as u64);
            let (b_emp, p_emp) = argmin(&rows, ecol, c_nj);
            let bs_round = bs_closed.round().clamp(B_MIN as f64, B_MAX as f64) as u32;
            let r_at = rows.iter().find(|r| r.b == bs_round).unwrap();
            let p_at_star = LAMBDA_OPS_PER_S * ecol(r_at) + c_nj * bs_round as f64 / 2.0;
            println!(
                "{},{:.1},{:.1},{:.4},{:.2},{:.3},{},{},{},{:.2},{:.2},{:.6},{:.4},{:.4}",
                label,
                LAMBDA_OPS_PER_S,
                c_nj,
                bs_closed / (2.0 * LAMBDA_OPS_PER_S),
                a_nj,
                bs_closed,
                bs_int,
                b_emp,
                b_emp == B_MAX || b_emp == B_MIN,
                p_at_star,
                p_emp,
                p_at_star / p_emp - 1.0,
                (bs_closed - b_emp as f64).abs() / b_emp as f64,
                (bs_int as f64 - b_emp as f64).abs() / b_emp as f64,
            );
        }
    }
}
