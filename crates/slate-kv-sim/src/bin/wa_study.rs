use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::HashMap;

fn zipf_cdf(n: usize, s: f64) -> Vec<f64> {
    let mut cdf = Vec::with_capacity(n);
    let mut sum = 0.0;
    for i in 1..=n {
        let w = 1.0 / (i as f64).powf(s);
        sum += w;
        cdf.push(sum);
    }
    for val in &mut cdf {
        *val /= sum;
    }
    cdf
}

fn simulate_wa(
    n_keys: usize,
    u_target: f64,
    skew_s: f64,
    n_ops: usize,
    seg_recs: usize,
    hot_cold: bool,
) -> (f64, f64) {
    let cdf = zipf_cdf(n_keys, skew_s);
    let mut rng = StdRng::seed_from_u64(4242);

    let mut cap_segs = (n_keys as f64 / (u_target * seg_recs as f64)).ceil() as usize;
    if cap_segs < 8 {
        cap_segs = 8;
    }
    if hot_cold {
        cap_segs += 2;
    }

    let mut segs = vec![Vec::new(); cap_segs];
    let mut live_ct = vec![0; cap_segs];
    let mut live_seg = HashMap::new();
    let mut free: Vec<usize> = (0..cap_segs).collect();

    let mut hot_head = free.pop().unwrap();
    let mut cold_head = if hot_cold {
        free.pop().unwrap()
    } else {
        hot_head
    };

    let cap_records = cap_segs * seg_recs;
    let mut stats_user = 0;
    let mut stats_copy = 0;

    let reserve = if hot_cold { 3 } else { 2 };

    let place = |key: usize,
                 is_copy: bool,
                 segs: &mut Vec<Vec<usize>>,
                 live_ct: &mut Vec<usize>,
                 live_seg: &mut HashMap<usize, usize>,
                 hot_head: &mut usize,
                 cold_head: &mut usize,
                 free: &mut Vec<usize>,
                 stats_user: &mut usize,
                 stats_copy: &mut usize| {
        let h = if hot_cold && is_copy {
            if segs[*cold_head].len() >= seg_recs {
                *cold_head = free.pop().unwrap();
            }
            *cold_head
        } else {
            if segs[*hot_head].len() >= seg_recs {
                *hot_head = free.pop().unwrap();
            }
            *hot_head
        };

        if let Some(&old) = live_seg.get(&key) {
            live_ct[old] -= 1;
        }
        segs[h].push(key);
        live_ct[h] += 1;
        live_seg.insert(key, h);

        if is_copy {
            *stats_copy += 1;
        } else {
            *stats_user += 1;
        }
    };

    let gc = |segs: &mut Vec<Vec<usize>>,
              live_ct: &mut Vec<usize>,
              live_seg: &mut HashMap<usize, usize>,
              hot_head: &mut usize,
              cold_head: &mut usize,
              free: &mut Vec<usize>,
              stats_user: &mut usize,
              stats_copy: &mut usize| {
        let mut guard = 0;
        while free.len() < reserve {
            guard += 1;
            if guard > 4 * cap_segs {
                break;
            }
            let mut best = None;
            let mut best_live = usize::MAX;
            for s in 0..cap_segs {
                if s == *hot_head || s == *cold_head || segs[s].is_empty() {
                    continue;
                }
                if live_ct[s] < best_live {
                    best_live = live_ct[s];
                    best = Some(s);
                }
            }
            if let Some(best_seg) = best {
                let keys = segs[best_seg].clone();
                for k in keys {
                    if live_seg.get(&k) == Some(&best_seg) {
                        place(
                            k, true, segs, live_ct, live_seg, hot_head, cold_head, free,
                            stats_user, stats_copy,
                        );
                    }
                }
                segs[best_seg].clear();
                live_ct[best_seg] = 0;
                free.push(best_seg);
            } else {
                break;
            }
        }
    };

    let need_gc =
        |hot_head: usize, cold_head: usize, free: &Vec<usize>, segs: &Vec<Vec<usize>>| -> bool {
            free.len() < reserve
                && (segs[hot_head].len() >= seg_recs
                    || (hot_cold && segs[cold_head].len() >= seg_recs))
        };

    for k in 0..n_keys {
        if need_gc(hot_head, cold_head, &free, &segs) {
            gc(
                &mut segs,
                &mut live_ct,
                &mut live_seg,
                &mut hot_head,
                &mut cold_head,
                &mut free,
                &mut stats_user,
                &mut stats_copy,
            );
        }
        place(
            k,
            false,
            &mut segs,
            &mut live_ct,
            &mut live_seg,
            &mut hot_head,
            &mut cold_head,
            &mut free,
            &mut stats_user,
            &mut stats_copy,
        );
    }

    for _ in 0..n_ops {
        let r = rng.gen::<f64>();
        let mut k = 0;
        for (i, &c) in cdf.iter().enumerate() {
            if r <= c {
                k = i;
                break;
            }
        }

        if need_gc(hot_head, cold_head, &free, &segs) {
            gc(
                &mut segs,
                &mut live_ct,
                &mut live_seg,
                &mut hot_head,
                &mut cold_head,
                &mut free,
                &mut stats_user,
                &mut stats_copy,
            );
        }
        place(
            k,
            false,
            &mut segs,
            &mut live_ct,
            &mut live_seg,
            &mut hot_head,
            &mut cold_head,
            &mut free,
            &mut stats_user,
            &mut stats_copy,
        );
    }

    let wa = (stats_user + stats_copy) as f64 / stats_user as f64;
    let meas_u = live_seg.len() as f64 / cap_records as f64;

    (wa, meas_u)
}

fn run_wa_study() {
    let n_keys = 2000;
    let n_ops = 40000;
    let us = [0.5, 0.6, 0.7, 0.8, 0.9];
    let skews = [0.0, 0.6, 0.9, 1.2];

    println!("u,s,gc_type,wa,meas_u");

    let mut failed = 0;

    for &s in &skews {
        for &u in &us {
            let (wa_emp, mu_emp) = simulate_wa(n_keys, u, s, n_ops, 64, false);
            let (wa_hc, mu_hc) = simulate_wa(n_keys, u, s, n_ops, 64, true);

            println!("{:.2},{:.1},single,{:.2},{:.3}", u, s, wa_emp, mu_emp);
            println!("{:.2},{:.1},hot_cold,{:.2},{:.3}", u, s, wa_hc, mu_hc);

            // Assertions
            if u <= 0.8 {
                let limit = 1.0 / (1.0 - mu_emp);
                if wa_emp > limit + 0.05 {
                    println!(
                        "FAIL: single WA {:.2} > model {:.2} at u={}",
                        wa_emp, limit, u
                    );
                    failed += 1;
                }
                let limit_hc = 1.0 / (1.0 - mu_hc);
                if wa_hc > limit_hc + 0.05 {
                    println!(
                        "FAIL: hot_cold WA {:.2} > model {:.2} at u={}",
                        wa_hc, limit_hc, u
                    );
                    failed += 1;
                }
            }
            if (u - 0.9).abs() < 0.01 {
                // hot/cold <= baseline at u = 0.89 (0.9 here)
                if wa_hc > wa_emp {
                    println!(
                        "FAIL: hot_cold WA {:.2} > baseline WA {:.2} at u=0.9, s={}",
                        wa_hc, wa_emp, s
                    );
                    failed += 1;
                }
            }
            // "skew helps (WA(s=1.2) <= WA(s=0) at fixed u)" -> verified below
        }
    }

    for &u in &us {
        let (wa_s0, _) = simulate_wa(n_keys, u, 0.0, n_ops, 64, true);
        let (wa_s12, _) = simulate_wa(n_keys, u, 1.2, n_ops, 64, true);
        // Skew helps at high utilizations for hot_cold
        if u >= 0.8 && wa_s12 > wa_s0 {
            println!(
                "FAIL: skew didn't help hot/cold: WA(s=1.2)={:.2} > WA(s=0)={:.2} at u={}",
                wa_s12, wa_s0, u
            );
            failed += 1;
        }
    }

    if failed > 0 {
        panic!("wa_study failed {} assertions", failed);
    } else {
        println!("wa_study passed all assertions!");
    }
}

fn main() {
    run_wa_study();
}
