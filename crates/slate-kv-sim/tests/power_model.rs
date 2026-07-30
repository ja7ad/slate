//! Energy model arithmetic.
//!
//! `power::report` converts byte and erase counters into an energy estimate,
//! and the commit-batch optimum in docs/specification.md is derived from it.
//! It had no test at all, so a units error — the model mixes nJ, µJ and mJ in
//! one expression — would have propagated silently into published numbers.
//!
//! These tests pin each term independently by zeroing the others, so a
//! regression names the term it broke rather than shifting one opaque total.

use slate_kv_sim::power::{report, PowerModel, Stats};

fn zero() -> Stats {
    Stats::default()
}

#[test]
fn an_idle_device_costs_nothing() {
    let r = report(&zero(), &PowerModel::default());
    assert_eq!(r.m_joules, 0.0);
}

#[test]
fn the_report_is_labelled_as_an_estimate() {
    // The label is what stops a modelled figure being quoted as a measurement.
    assert_eq!(report(&zero(), &PowerModel::default()).label, "ESTIMATED");
}

#[test]
fn write_energy_scales_with_bytes_at_beta() {
    // Isolate the write term: no erases, no wakes, and beta chosen so the CPU
    // term is zero.
    let m = PowerModel {
        beta_nj_per_byte: 200,
        erase_uj_per_block: 0,
        wake_uj: 0,
        cpu_nj_per_cycle_q10: 0,
        aead_cycles_per_byte: 0,
    };
    let s = Stats {
        user_bytes: 1_000_000,
        ..zero()
    };
    // 1e6 B * 200 nJ/B = 2e8 nJ = 200 mJ (report divides nJ by 1e6).
    assert!((report(&s, &m).m_joules - 200.0).abs() < 1e-9);
}

#[test]
fn every_byte_bucket_the_model_reads_is_weighted_identically() {
    let m = PowerModel {
        beta_nj_per_byte: 1,
        erase_uj_per_block: 0,
        wake_uj: 0,
        cpu_nj_per_cycle_q10: 0,
        aead_cycles_per_byte: 0,
    };
    let n = 4096;
    let each = [
        Stats {
            user_bytes: n,
            ..zero()
        },
        Stats {
            gc_bytes: n,
            ..zero()
        },
        Stats {
            parity_bytes: n,
            ..zero()
        },
        Stats {
            ckpt_bytes: n,
            ..zero()
        },
    ];
    let first = report(&each[0], &m).m_joules;
    for (i, s) in each.iter().enumerate() {
        assert!(
            (report(s, &m).m_joules - first).abs() < 1e-12,
            "bucket {i} is weighted differently from user_bytes"
        );
    }
}

#[test]
fn erase_energy_uses_microjoules_per_block() {
    let m = PowerModel {
        beta_nj_per_byte: 0,
        erase_uj_per_block: 5000,
        wake_uj: 0,
        cpu_nj_per_cycle_q10: 0,
        aead_cycles_per_byte: 0,
    };
    let s = Stats {
        erases: 10,
        ..zero()
    };
    // 10 * 5000 uJ = 50 000 uJ = 5e7 nJ = 50 mJ.
    assert!((report(&s, &m).m_joules - 50.0).abs() < 1e-9);
}

#[test]
fn wake_energy_uses_microjoules_per_wake() {
    let m = PowerModel {
        beta_nj_per_byte: 0,
        erase_uj_per_block: 0,
        wake_uj: 1000,
        cpu_nj_per_cycle_q10: 0,
        aead_cycles_per_byte: 0,
    };
    let s = Stats { wakes: 7, ..zero() };
    // 7 * 1000 uJ = 7000 uJ = 7e6 nJ = 7 mJ.
    assert!((report(&s, &m).m_joules - 7.0).abs() < 1e-9);
}

#[test]
fn cpu_energy_applies_the_q10_fixed_point_scale() {
    // cpu_nj = bytes * cycles_per_byte * nj_per_cycle_q10 / 1024
    let m = PowerModel {
        beta_nj_per_byte: 0,
        erase_uj_per_block: 0,
        wake_uj: 0,
        cpu_nj_per_cycle_q10: 1024, // == 1 nJ/cycle
        aead_cycles_per_byte: 24,
    };
    let s = Stats {
        user_bytes: 1_000_000,
        ..zero()
    };
    // 1e6 B * 24 cycles/B * 1 nJ = 2.4e7 nJ = 24 mJ.
    assert!((report(&s, &m).m_joules - 24.0).abs() < 1e-9);
}

#[test]
fn terms_are_additive() {
    let m = PowerModel::default();
    let a = Stats {
        user_bytes: 50_000,
        ..zero()
    };
    let b = Stats {
        erases: 3,
        wakes: 2,
        ..zero()
    };
    let both = Stats {
        user_bytes: 50_000,
        erases: 3,
        wakes: 2,
        ..zero()
    };
    let sum = report(&a, &m).m_joules + report(&b, &m).m_joules;
    assert!((report(&both, &m).m_joules - sum).abs() < 1e-9);
}

#[test]
fn energy_is_monotone_in_every_input() {
    let m = PowerModel::default();
    let base = Stats {
        user_bytes: 10_000,
        erases: 1,
        wakes: 1,
        ..zero()
    };
    let e0 = report(&base, &m).m_joules;
    for bump in [
        Stats {
            user_bytes: 20_000,
            ..base.clone()
        },
        Stats {
            erases: 2,
            ..base.clone()
        },
        Stats {
            wakes: 2,
            ..base.clone()
        },
        Stats {
            gc_bytes: 5_000,
            ..base.clone()
        },
    ] {
        assert!(
            report(&bump, &m).m_joules > e0,
            "more work must never cost less energy"
        );
    }
}

/// KNOWN GAP, pinned deliberately so it cannot change unnoticed.
///
/// `power::Stats` has no `marker_bytes` field and `report` therefore never
/// charges for commit markers, while `slate_kv_core::Metrics` does track them
/// and includes them in `flash_bytes()`. Each commit programs two marker
/// pages, so the omitted term scales as 1/B and dominates at small batch
/// sizes — exactly the regime the energy-optimum argument depends on.
///
/// The specification uses `SimFlash::stats.bytes_programmed` (physical ground
/// truth) rather than this function for that reason. This test documents the
/// discrepancy; if `marker_bytes` is ever added here, it will fail and should
/// be replaced with a positive assertion.
#[test]
fn report_does_not_charge_for_commit_markers() {
    let m = PowerModel::default();
    let mut s = Stats {
        user_bytes: 1024,
        commits: 100,
        ..zero()
    };
    let with_commits = report(&s, &m).m_joules;
    s.commits = 0;
    let without_commits = report(&s, &m).m_joules;
    assert_eq!(
        with_commits, without_commits,
        "report() still ignores commit count; if this now differs, the marker \
         term was added and this test should assert the new behaviour instead"
    );
}
