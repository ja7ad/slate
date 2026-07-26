use rand::{rngs::StdRng, Rng, SeedableRng};
use slate_kv_core::sched::b_star;
use slate_kv_sim::power::{report, PowerModel};
use slate_kv_sim::sim_db::{Db, KeySource, Options};
use slate_kv_sim::{SimCounter, SimFlash};

fn run_energy_check() {
    let model = PowerModel::default();

    // Test parameters
    let lam_ops_per_sec = 10.0;
    let lam_q10 = (lam_ops_per_sec * 1024.0) as u64;

    // We need to determine the physical A (fixed cost).
    // A commit involves:
    // 1 wake: 1000 uJ = 1_000_000 nJ
    // 1 commit write: approx 2 blocks of 64 bytes?
    // Let's just measure it by running B=1 vs B=2 and looking at the difference.

    let get_energy_for_b = |b: u32| -> f64 {
        let opts = Options {
            auto_b: false,
            b_commit: b,
            ..Default::default()
        };
        // Turn off GC so it doesn't pollute the numbers (u=0.1)

        let n_ops = 10000;
        let flash = SimFlash::new(opts.capacity, 256, 4096);
        let counter = SimCounter::new(100_000);
        let db = Db::open(KeySource::Bytes([0u8; 32]), opts.clone(), flash, counter).unwrap();

        let mut rng = StdRng::seed_from_u64(1234);
        for _ in 0..n_ops {
            let mut key = [0u8; 8];
            rng.fill(&mut key);
            let val = [0u8; 16];
            db.put(&key, &val).unwrap();
        }

        // Report power
        let metrics = db.stats();
        let p_rep = report(&metrics, &model);
        // Returns total Energy in mJ
        let m_joules = p_rep.m_joules;
        let nj_total = m_joules * 1_000_000.0;

        // Energy per op in nJ
        nj_total / n_ops as f64
    };

    let e_b1 = get_energy_for_b(1);
    let e_b10 = get_energy_for_b(10);

    // E(B) = A/B + E_payload
    // E(1) = A + E_payload
    // E(10) = A/10 + E_payload
    // E(1) - E(10) = A * 0.9 => A = (E(1) - E(10)) / 0.9
    let a_nj = (e_b1 - e_b10) / 0.9;
    let a_uj = (a_nj / 1000.0) as u64;

    println!("Measured A: {} uJ (fixed cost per commit)", a_uj);

    // Let's set c based on the doc. c = A / (2 * lambda * t_max^2)
    // Or let's just pick a c that makes B* = 27 roughly.
    // 27 = sqrt(2 * lam_q10/1024 * a_uj * 1000 / c) => c = 2 * lam * A / B^2
    let c_nj = ((2.0 * lam_ops_per_sec * (a_uj as f64 * 1000.0)) / (27.0 * 27.0)) as u64;

    let bs = b_star(lam_q10, a_uj, c_nj);
    println!("Computed B* = {}", bs);

    // Sweep B
    let mut min_total_cost = f64::MAX;
    let mut min_b = 1;

    let mut p_bstar = 0.0;
    let mut p_2bstar = 0.0;

    for b in (bs / 4).max(1)..(bs * 4) {
        let e_phys_per_op = get_energy_for_b(b);
        // physical power = e_phys_per_op * lambda
        let p_phys = e_phys_per_op * lam_ops_per_sec;

        // holding power = c * B / 2
        let p_hold = (c_nj as f64 * b as f64) / 2.0;

        let p_total = p_phys + p_hold;

        if p_total < min_total_cost {
            min_total_cost = p_total;
            min_b = b;
        }

        if b == bs {
            p_bstar = p_total;
        }
        if b == bs * 2 {
            p_2bstar = p_total;
        }

        // println!("B={}, P_phys={}, P_hold={}, P_tot={}", b, p_phys, p_hold, p_total);
    }

    println!("Minimum total power at B={}, computed B*={}", min_b, bs);
    if (min_b as i32 - bs as i32).abs() > 1 {
        panic!(
            "Minimum total power B={} is not within +/- 1 of B*={}",
            min_b, bs
        );
    }

    let ratio = p_2bstar / p_bstar;
    println!("P(2B*) / P(B*) = {:.3} (limit 1.25)", ratio);
    if ratio > 1.25 + 0.01 {
        panic!("P(2B*) / P(B*) is too high: {:.3}", ratio);
    }

    println!("energy_check passed!");
}

fn main() {
    run_energy_check();
}
