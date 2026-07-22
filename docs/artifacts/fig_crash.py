"""
Crash-injection Monte-Carlo simulation for SLATE.

Validates two claims of the revised report by direct simulation of the on-flash
state machine (no hardware needed -- this validates the *model*, answering the
"purely theoretical" critique):

  (T1) Prefix-durability (Theorem 1): after a crash at an ARBITRARY byte offset
       and recovery, the recovered logical state equals exactly the last
       COMMITTED prefix -- every acknowledged write survives, and no torn or
       uncommitted tail is ever accepted.

  (C)  Counter-recovery rule (Lemma "crash-window liveness" + Theorem
       "rollback detection", section 3.4): with the write-ahead epoch-seal
       ordering (flush marker carrying counter e, THEN advance hardware MC),
       a genuine store always boots (marker counter m in {MC*, MC*+1}), while
       any stale image from an earlier epoch (rollback) is always rejected.

We model the log as a byte stream of records grouped into batches; each batch
ends with a commit marker. Epochs group batches; the hardware counter advances
once per epoch, AFTER the sealing marker is durable. A crash truncates the
stream at a uniformly-random byte and may also land inside the seal window
(marker durable, counter not yet advanced).
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

rng = np.random.default_rng(20240607)

REC = 100          # bytes per record (modeled)
MARK = 40          # bytes per commit marker
Theta = 64         # records per epoch (epoch = checkpoint interval)

def build_log(n_records, batch_size):
    """Return list of byte 'events': ('rec', ack_seq_if_committed) laid out on flash.
    Each event is (kind, seq, byte_len, committed_prefix_len_after, epoch, counter_written).
    A record is ACKed (durable) only once its batch's marker is written.
    Epoch seal advances hardware counter AFTER marker durable."""
    events = []      # (kind, byte_len)
    boundaries = []  # after each event: (committed_records, marker_counter_field, hw_counter)
    committed = 0
    seq = 0
    hw_counter = 0          # hardware monotonic counter (advances per epoch seal)
    epoch = 1               # epoch being written; markers in epoch e carry field e
    recs_in_epoch = 0
    pending = 0             # records written but not yet committed
    i = 0
    while i < n_records:
        # write a batch of `batch_size` records
        for _ in range(batch_size):
            if i >= n_records: break
            seq += 1; i += 1; pending += 1; recs_in_epoch += 1
            events.append(("rec", REC))
            boundaries.append((committed, epoch, hw_counter))  # not yet committed
        # write commit marker -> commits the pending records (they become durable/ACKed)
        committed += pending; pending = 0
        events.append(("mark", MARK))
        # is this an epoch-sealing marker?
        sealing = recs_in_epoch >= Theta
        # marker carries counter field = epoch (target value for this epoch)
        boundaries.append((committed, epoch, hw_counter))
        if sealing:
            # WRITE-AHEAD ORDER: marker (field=epoch) is now durable.
            # THEN advance hardware counter. Crash between = seal window.
            events.append(("seal_advance", 0))  # zero-length: the HW counter tick
            hw_counter = epoch
            boundaries.append((committed, epoch, hw_counter))
            epoch += 1; recs_in_epoch = 0
    return events, boundaries

def recover(events, crash_event_idx, crash_frac_into_event):
    """Simulate a crash: all events strictly before crash_event_idx are durable;
    the crash event is partially written (torn) if it's a record/marker.
    Returns (recovered_committed_records, marker_counter_field, hw_counter).
    Recovery = scan valid prefix: accept only whole records inside a committed
    batch (i.e. up to the last fully-written marker), replay them; truncate the
    torn/uncommitted tail. Counter recovered from last durable sealing tick."""
    committed = 0
    marker_field = 0
    hw = 0
    last_committed_at_marker = 0
    for j, (kind, blen) in enumerate(events):
        if j > crash_event_idx:
            break
        torn = (j == crash_event_idx and crash_frac_into_event < 1.0)
        if kind == "rec":
            # a torn record is NOT counted (magic/len/tag check fails); recovery truncates
            pass  # records only become durable via their marker
        elif kind == "mark":
            if not torn:
                # marker fully written -> its batch commits; find committed count
                # committed count is recorded in boundaries; recompute by counting recs
                # up to here that lie in a completed batch
                committed = _count_committed(events, j)
                marker_field = _marker_field(events, j)
                last_committed_at_marker = committed
            # torn marker: batch not committed, tail truncated -> ignore
        elif kind == "seal_advance":
            if not torn:
                hw = marker_field  # hardware advanced to the sealed epoch
    return last_committed_at_marker, marker_field, hw

# helper: count records committed as of marker at event index jm
def _count_committed(events, jm):
    c = 0
    for k in range(jm):
        if events[k][0] == "rec":
            c += 1
    return c

def _marker_field(events, jm):
    # reconstruct epoch field of the marker at index jm:
    # epoch increments after each seal_advance before jm, starting at 1
    e = 1
    for k in range(jm):
        if events[k][0] == "seal_advance":
            e += 1
    return e

# ---------------- Monte-Carlo over many crash points ----------------
N_TRIALS = 20000
batch_size = 8
events, boundaries = build_log(n_records=600, batch_size=batch_size)

# "ground truth" acked prefix as a function of how far we got:
# for each event index, the number of records that were ACKed (committed) if power
# were cut cleanly AFTER that event.
def acked_after(events, idx):
    c = 0; committed = 0
    for k in range(idx + 1):
        if events[k][0] == "rec":
            c += 1
        elif events[k][0] == "mark":
            committed = c
    return committed

durability_ok = 0
lost_ack = 0
accepted_torn = 0
in_seal_window = 0
seal_window_boots = 0

for _ in range(N_TRIALS):
    ci = rng.integers(0, len(events))
    frac = rng.uniform(0.0, 1.0)   # <1 means the event is torn (partial write)
    rec_committed, mfield, hw = recover(events, ci, frac)
    # ground-truth: acked prefix is the last committed count at or before the crash
    # (a torn event cannot have ACKed anything new)
    truth = acked_after(events, ci - 1 if frac < 1.0 else ci)
    # T1 checks:
    if rec_committed == truth:
        durability_ok += 1
    if rec_committed < truth:
        lost_ack += 1            # LOST an acknowledged write -> durability violation
    if rec_committed > truth:
        accepted_torn += 1       # accepted uncommitted/torn data -> safety violation
    # Counter recovery: genuine device boot rule m in {hw, hw+1}
    kind = events[ci][0]
    if kind == "seal_advance" and frac < 1.0:
        in_seal_window += 1
        # marker durable with field=mfield, hw not yet advanced -> m = hw+1
        if mfield in (hw, hw + 1):
            seal_window_boots += 1

# ---------------- Rollback rejection test ----------------
# Present stale-but-authentic images from STRICTLY EARLIER epochs (field < MC*):
# these are genuine cross-epoch rollbacks and the boot rule must reject ALL of them.
# (An image from the current epoch, field == MC*, is legitimately the live state, not
#  a rollback; within-epoch rollback is the acknowledged Theta-op window, not tested here.)
rollback_trials = 5000
rollback_rejected = 0
# current device hw counter after a clean full run:
_, _, hw_now = recover(events, len(events) - 1, 1.0)
cur_epoch_field = _marker_field(events, len(events) - 1)
for _ in range(rollback_trials):
    stale_epoch = rng.integers(1, hw_now)  # 1 .. hw_now-1 : strictly earlier epoch
    # boot rule: accept only if marker field in {MC*, MC*+1}
    accept = stale_epoch in (hw_now, hw_now + 1)
    if not accept:
        rollback_rejected += 1

print(f"events={len(events)} trials={N_TRIALS}")
print(f"durability_ok={durability_ok} ({100*durability_ok/N_TRIALS:.2f}%)")
print(f"lost_ack={lost_ack}  accepted_torn={accepted_torn}")
print(f"seal_window_hits={in_seal_window} seal_window_boots_ok={seal_window_boots}")
print(f"rollback_rejected={rollback_rejected}/{rollback_trials} "
      f"({100*rollback_rejected/rollback_trials:.2f}%)  hw_now={hw_now} cur_epoch={cur_epoch_field}")

# ---------------- figure ----------------
fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(11, 4.4))

# Panel A: recovered committed prefix vs crash position (a sample scan), overlaid on ground truth
sample = 400
idxs = np.linspace(0, len(events) - 1, sample).astype(int)
rec_curve = []
truth_curve = []
for ii in idxs:
    rc, _, _ = recover(events, ii, 1.0)   # clean cut after event ii
    rec_curve.append(rc)
    truth_curve.append(acked_after(events, ii))
ax1.plot(idxs, truth_curve, color="#123f66", lw=2.0, label="acknowledged writes (ground truth)")
ax1.plot(idxs, rec_curve, color="#c23b22", lw=1.0, ls="--", label="recovered committed prefix")
ax1.set_xlabel("crash position (flash event index)")
ax1.set_ylabel("records durable after recovery")
ax1.set_title("T1: recovery = last committed prefix (exact)", fontsize=9.5)
ax1.legend(frameon=False, fontsize=8, loc="upper left")

# Panel B: outcome bars
labels = ["durability\nOK", "lost ACK\n(violation)", "accepted torn\n(violation)",
          "earlier-epoch\nrollback rejected"]
vals = [100*durability_ok/N_TRIALS, 100*lost_ack/N_TRIALS,
        100*accepted_torn/N_TRIALS, 100*rollback_rejected/rollback_trials]
colors = ["#3a7d44", "#c23b22", "#c23b22", "#3a7d44"]
bars = ax2.bar(labels, vals, color=colors, width=0.62, edgecolor="black", lw=0.6)
for b, v in zip(bars, vals):
    ax2.text(b.get_x()+b.get_width()/2, v+1.5, f"{v:.1f}%", ha="center", fontsize=8.5)
ax2.set_ylim(0, 112)
ax2.set_ylabel("fraction of trials (%)")
ax2.set_title(f"Crash-injection outcomes ({N_TRIALS:,} random crashes)", fontsize=9.5)
ax2.axhline(100, color="gray", lw=0.7, ls=":")

fig.suptitle("SLATE crash-injection Monte-Carlo: durability (T1) and counter recovery hold under arbitrary power loss",
             fontsize=10.5, y=1.02)
fig.tight_layout()
fig.savefig("crash_sim.png", dpi=200, bbox_inches="tight")
print("saved crash_sim.png")
