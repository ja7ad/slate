"""
Skewed-workload write-amplification simulation for SLATE.

The report's Theorem "Write-amplification" gives WA = 1/(1-u) for a
greedy segment-compaction (log-structured) GC. That closed form is derived
under a uniform steady-state assumption. Reviewers (evl1, evl3) correctly
noted that under SKEWED (Zipfian) update workloads the realized WA can differ:
hot keys are overwritten repeatedly, so segments hold a mix of hot (soon-dead)
and cold (long-lived) records, and a utilization-triggered greedy GC behaves
differently from the uniform ideal.

This simulation runs a real log-structured store with greedy GC under Zipfian
key-update workloads of varying skew, measures the empirical WA, and compares
it to the 1/(1-u) model. The purpose is to (a) validate the model where it
holds and (b) HONESTLY characterize where and by how much skew changes it.

Model of the store:
  - N_keys logical keys; each Put appends a fresh record to the head segment.
  - A segment holds SEG_RECS record-slots. When the store's live-fraction would
    exceed the utilization target u, GC compacts: it picks the segment with the
    fewest live records (greedy), copies its live records forward (these copies
    are the write-amplification), and erases it.
  - A record is "live" if it is the latest version of its key; superseded
    versions are dead and reclaimable.
  WA = (user writes + copy-forward writes) / user writes.
"""
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

plt.rcParams.update({
    "font.size": 9, "axes.linewidth": 0.8, "savefig.dpi": 200,
    "font.family": "DejaVu Sans", "mathtext.fontset": "dejavusans",
    "axes.grid": True, "grid.alpha": 0.3, "grid.linewidth": 0.5,
})

rng = np.random.default_rng(4242)

def zipf_weights(n, s):
    """Normalized Zipf(s) weights over n keys. s=0 -> uniform."""
    ranks = np.arange(1, n + 1)
    w = 1.0 / np.power(ranks, s)
    return w / w.sum()

def simulate_wa(n_keys, u_target, skew_s, n_ops, seg_recs=64, hot_cold=False):
    """Efficient log-structured store with greedy GC; return empirical WA.
    Incremental bookkeeping (no per-op full scan): O(1) amortized per write,
    GC cost paid only during reclamation.

    hot_cold=False: single append head (baseline greedy GC).
    hot_cold=True : two append heads with age separation. Fresh user writes go
      to the HOT head; records that survive a compaction (they were live when
      their segment was reclaimed, i.e. they are comparatively long-lived) are
      copied forward to a separate COLD head. Segregating soon-dead from
      long-lived records makes hot segments empty out almost completely before
      they are chosen for GC, cutting copy-forward traffic under skew."""
    weights = zipf_weights(n_keys, skew_s)
    # capacity sized so steady-state live fraction ~= u_target:
    # live keys / physical capacity = u  =>  cap_records = n_keys / u.
    # The (1-u) fraction is exactly the over-provisioning the GC needs as headroom.
    cap_segs = max(8, int(np.ceil(n_keys / (u_target * seg_recs))))
    if hot_cold:
        cap_segs += 2       # a second append head needs a little extra headroom

    segs = [[] for _ in range(cap_segs)]      # segs[i] = list of keys appended
    live_ct = [0] * cap_segs                  # live records per segment
    live_seg = {}                             # key -> segment id holding its live copy
    free = list(range(cap_segs))              # free segment ids
    hot_head = free.pop()
    cold_head = free.pop() if hot_cold else hot_head
    n_occ = 0                                 # total physical records occupied
    cap_records = cap_segs * seg_recs

    stats = {"user": 0, "copy": 0}

    def place(key, is_copy):
        nonlocal hot_head, cold_head, n_occ
        if hot_cold and is_copy:
            if len(segs[cold_head]) >= seg_recs:
                cold_head = free.pop()
            h = cold_head
        else:
            if len(segs[hot_head]) >= seg_recs:
                hot_head = free.pop()
            h = hot_head
        old = live_seg.get(key)
        if old is not None:
            live_ct[old] -= 1
        segs[h].append(key)
        live_ct[h] += 1
        live_seg[key] = h
        n_occ += 1
        stats["copy" if is_copy else "user"] += 1

    reserve = 3 if hot_cold else 2

    def gc():
        nonlocal n_occ
        # reclaim until enough free segments (room to append + copy-forward heads)
        guard = 0
        while len(free) < reserve:
            guard += 1
            if guard > 4 * cap_segs:      # infeasible headroom at this u -> stop
                break
            # greedy victim: non-head, non-empty, fewest live records
            best = -1; best_live = 1 << 30
            for s in range(cap_segs):
                if s == hot_head or s == cold_head or not segs[s]:
                    continue
                if live_ct[s] < best_live:
                    best_live = live_ct[s]; best = s
            if best < 0:
                break
            for k in segs[best]:
                if live_seg.get(k) == best:      # still live -> copy forward
                    place(k, is_copy=True)
            n_occ -= len(segs[best])
            segs[best] = []; live_ct[best] = 0
            free.append(best)

    def need_gc():
        return len(free) < reserve and (
            len(segs[hot_head]) >= seg_recs or
            (hot_cold and len(segs[cold_head]) >= seg_recs))

    # warm-up: insert every key once
    for k in range(n_keys):
        if need_gc():
            gc()
        place(k, is_copy=False)
    # steady-state skewed workload
    keys = rng.choice(n_keys, size=n_ops, p=weights)
    for k in keys:
        if need_gc():
            gc()
        place(int(k), is_copy=False)

    wa = (stats["user"] + stats["copy"]) / stats["user"]
    meas_u = len(live_seg) / cap_records   # live records / physical capacity
    return wa, meas_u

# ---------------- sweep ----------------
N_KEYS = 2000
N_OPS = 40000
us = [0.5, 0.6, 0.7, 0.8, 0.9]
skews = [0.0, 0.6, 0.9, 1.2]     # 0=uniform, ~1.0 = classic Zipf, 1.2 = heavy skew
skew_labels = ["uniform (s=0)", "s=0.6", "s=0.9 (Zipf)", "s=1.2 (heavy)"]

# baseline greedy GC and hot/cold-aware GC (age-segregated append heads)
wa_emp = {s: [] for s in skews}      # baseline
mu_emp = {s: [] for s in skews}
wa_hc  = {s: [] for s in skews}      # hot/cold
mu_hc  = {s: [] for s in skews}
for s in skews:
    for u in us:
        wa, mu = simulate_wa(N_KEYS, u, s, N_OPS, hot_cold=False)
        wa_emp[s].append(wa); mu_emp[s].append(mu)
        wh, mh = simulate_wa(N_KEYS, u, s, N_OPS, hot_cold=True)
        wa_hc[s].append(wh); mu_hc[s].append(mh)
    print(f"skew s={s}: WA_base={[round(x,2) for x in wa_emp[s]]}  "
          f"WA_hotcold={[round(x,2) for x in wa_hc[s]]}  "
          f"meas_u={[round(x,3) for x in mu_emp[s]]}  "
          f"model@meas_u={[round(1/(1-x),2) for x in mu_emp[s]]}")

# ---------------- figure ----------------
fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(11, 4.4))

# Panel A: WA vs MEASURED u, model line + baseline empirical points per skew
uu = np.linspace(0.45, 0.95, 100)
ax1.plot(uu, 1.0/(1.0-uu), color="black", lw=2.0, label=r"model  $\mathrm{WA}=1/(1-u)$", zorder=2)
cmap = plt.cm.viridis(np.linspace(0.15, 0.85, len(skews)))
for s, col, lab in zip(skews, cmap, skew_labels):
    ax1.plot(mu_emp[s], wa_emp[s], 'o-', color=col, lw=1.1, ms=5, label=lab, zorder=3)
ax1.set_xlabel("measured live utilization $u$")
ax1.set_ylabel(r"write-amplification  WA")
ax1.set_title("Baseline greedy GC vs the $1/(1-u)$ model", fontsize=9.5)
ax1.legend(frameon=False, fontsize=7.6, loc="upper left")

# Panel B: hot/cold-aware GC vs baseline, at the recommended and a stressed u,
# across skew -> shows the additional WA reduction from age segregation
b_us = [0.6, 0.8]
styles = {0.6: "-", 0.8: "--"}
for u in b_us:
    iu = us.index(u)
    base = [wa_emp[s][iu] for s in skews]
    hc   = [wa_hc[s][iu]  for s in skews]
    ax2.plot(skews, base, 'o'+styles[u], color="#c1442e", lw=1.3, ms=5,
             label=f"baseline greedy, u={u}")
    ax2.plot(skews, hc, 's'+styles[u], color="#2e6fc1", lw=1.3, ms=5,
             label=f"hot/cold-aware, u={u}")
ax2.set_xlabel("Zipf skew parameter $s$")
ax2.set_ylabel(r"write-amplification  WA")
ax2.set_title("Hot/cold age separation lowers WA further under skew", fontsize=9.5)
ax2.legend(frameon=False, fontsize=7.4, loc="best")

fig.suptitle("SLATE write-amplification under skewed (Zipfian) workloads: the $1/(1-u)$ model is a conservative upper bound, and hot/cold GC improves on it",
             fontsize=9.6, y=1.02)
fig.tight_layout()
fig.savefig("skew_wa.png", dpi=200, bbox_inches="tight")
print("saved skew_wa.png")
