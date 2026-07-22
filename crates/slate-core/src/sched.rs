//! sched
#![allow(missing_docs)]

use crate::config::SchedCfg;

/// Integer square root (u64), Newton's method — no_std, no float.
pub const fn isqrt(x: u64) -> u64 {
    if x < 2 {
        return x;
    }
    let mut r = 1u64 << ((64 - x.leading_zeros()).div_ceil(2)); // initial overestimate
    loop {
        let n = (r + x / r) / 2;
        if n >= r {
            return r;
        }
        r = n;
    }
}

/// EWMA op-rate estimator, fixed-point Q10 (rate in ops per 1024 s to keep
/// integer precision at low duty-cycled rates). alpha = 1/16 per sample.
pub struct RateEst {
    pub lam_q10: u64,
    pub last_ms: u64,
}

impl RateEst {
    pub fn new() -> Self {
        Self {
            lam_q10: 1024, // Assume 1 op/s initially to prevent zero rate issues
            last_ms: u64::MAX,
        }
    }

    pub fn on_op(&mut self, now_ms: u64) {
        if self.last_ms == u64::MAX {
            self.last_ms = now_ms;
            return;
        }
        let dt = (now_ms.saturating_sub(self.last_ms)).max(1);
        let inst_q10 = (1024 * 1000) / dt; // ops per 1024 s, this interval
        self.lam_q10 = self.lam_q10 - self.lam_q10 / 16 + inst_q10 / 16;
        self.last_ms = now_ms;
    }
}

impl Default for RateEst {
    fn default() -> Self {
        Self::new()
    }
}

/// B★ = sqrt(2·λ·A / c), then clamp: B = clamp(min(B★, λ·D), b_min, b_max).
/// Units: λ[q10 ops/s]·A[µJ]·1000/c[nJ/(op·s)] keeps everything in u64 with
/// the q10 scaling cancelling inside the sqrt.
pub fn b_star(lam_q10: u64, a_uj: u64, c_nj: u64) -> u32 {
    // 2λA/c = 2 · (lam_q10/1024) · (a_uj·1000 nJ) / c_nj
    let num = 2u64
        .saturating_mul(lam_q10)
        .saturating_mul(a_uj)
        .saturating_mul(1000);
    let b2 = num / (1024 * c_nj.max(1));
    isqrt(b2).max(1) as u32
}

pub struct Scheduler {
    pub cfg: SchedCfg,
    pub rate: RateEst,
    pub ops_since_commit: u32,
    pub oldest_pending_ms: u64,
}

impl Scheduler {
    pub fn new(cfg: SchedCfg) -> Self {
        Self {
            cfg,
            rate: RateEst::new(),
            ops_since_commit: 0,
            oldest_pending_ms: 0,
        }
    }

    /// Called after every append. Returns true ⇒ log.commit() now.
    pub fn on_append(&mut self, now_ms: u64) -> bool {
        self.rate.on_op(now_ms);
        self.ops_since_commit += 1;
        if self.ops_since_commit == 1 {
            self.oldest_pending_ms = now_ms;
        }
        let b = if self.cfg.auto_b {
            let t_ms = self.cfg.staleness_budget_ms as u64;
            // c_nj = (A_uj * 1000 * 1024 * 1000000) / (2 * lam_q10 * t_ms^2)
            // c_nj = (A_uj * 512_000_000_000) / (lam_q10 * t_ms^2)
            let c_nj = (self.cfg.fixed_cost_uj.saturating_mul(512_000_000_000))
                / (self.rate.lam_q10.max(1) * t_ms.saturating_mul(t_ms).max(1));

            let bs = b_star(self.rate.lam_q10, self.cfg.fixed_cost_uj, c_nj);
            // deadline clamp B ≤ λD (Thm 8.1 constrained case), λD in ops:
            let lam_d = (self.rate.lam_q10 * self.cfg.deadline_ms as u64) / (1024 * 1000);
            bs.min(lam_d.max(1) as u32)
        } else {
            self.cfg.b_commit
        };
        let b = b.clamp(self.cfg.b_min, self.cfg.b_max);
        // hard deadline: even if B not reached, no write waits past D (§8.2)
        self.ops_since_commit >= b
            || now_ms.saturating_sub(self.oldest_pending_ms) >= self.cfg.deadline_ms as u64
    }

    /// Called periodically by a timer. Returns true ⇒ log.commit() now.
    pub fn poll(&mut self, now_ms: u64) -> bool {
        if self.ops_since_commit == 0 {
            return false;
        }
        now_ms.saturating_sub(self.oldest_pending_ms) >= self.cfg.deadline_ms as u64
    }

    pub fn on_commit(&mut self) {
        self.ops_since_commit = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_isqrt() {
        assert_eq!(isqrt(0), 0);
        assert_eq!(isqrt(1), 1);
        assert_eq!(isqrt(2), 1);
        assert_eq!(isqrt(3), 1);
        assert_eq!(isqrt(4), 2);
        assert_eq!(isqrt(8), 2);
        assert_eq!(isqrt(9), 3);
        assert_eq!(isqrt(15), 3);
        assert_eq!(isqrt(16), 4);
        assert_eq!(isqrt(u64::MAX), 4294967295);

        for r in 0..1000 {
            let x = r * r;
            assert_eq!(isqrt(x), r);
            if r > 0 {
                assert_eq!(isqrt(x - 1), r - 1);
            }
            assert_eq!(isqrt(x + r), r);
        }
    }

    #[test]
    fn test_b_star() {
        // b_star(lam_q10: u64, a_uj: u64, c_nj: u64)
        // Check ESP32 default config
        let lam_q10 = 1024; // 1 op/s
        let a_uj = 400; // 400 uJ fixed cost
        let c_nj = 1000; // 1000 nJ holding cost
        let b = b_star(lam_q10, a_uj, c_nj);
        // B★ = sqrt(2 * 1 * 400,000 / 1000) = sqrt(800) = 28
        assert_eq!(b, 28);
    }

    #[test]
    fn test_scheduler_deadline() {
        let cfg = SchedCfg {
            auto_b: true,
            fixed_cost_uj: 400,
            staleness_budget_ms: 1000,
            deadline_ms: 1000,
            b_min: 1,
            b_max: 128,
            b_commit: 27,
        };
        let mut sched = Scheduler::new(cfg);
        let mut now_ms = 0;
        let mut uncommitted = 0;
        let mut oldest = 0;

        for i in 0..1000 {
            // Random-ish interval
            let dt = (isqrt(i as u64) % 500) + 1;
            now_ms += dt;

            if uncommitted == 0 {
                oldest = now_ms;
            }
            uncommitted += 1;

            if sched.on_append(now_ms) {
                // Must not exceed deadline
                assert!(
                    now_ms - oldest <= 1000,
                    "Deadline exceeded: {} - {} = {}",
                    now_ms,
                    oldest,
                    now_ms - oldest
                );
                sched.on_commit();
                uncommitted = 0;
            } else {
                // Not committed yet, check if deadline is violated
                assert!(
                    now_ms - oldest < 1000,
                    "Deadline violated without commit: {} - {} = {}",
                    now_ms,
                    oldest,
                    now_ms - oldest
                );
            }
        }
    }

    #[test]
    fn test_convexity() {
        let lam_q10 = 1024;
        let a_uj = 400;
        let c_nj = 1000;
        let bs = b_star(lam_q10, a_uj, c_nj);

        let power = |b: u32| -> u64 {
            let b = b as u64;
            (a_uj * 1000) / b + c_nj * b / 2
        };

        let p_star = power(bs);

        let mut min_p = p_star;
        let mut min_b = bs;
        for b in (bs / 4)..=(bs * 4) {
            let p = power(b.max(1));
            if p < min_p {
                min_p = p;
                min_b = b;
            }
        }
        assert!((min_b as i64 - bs as i64).abs() <= 1);

        let p_2bs = power(bs * 2);
        assert!(p_2bs <= (p_star * 125) / 100 + 10); // +10 nJ epsilon
    }
}
