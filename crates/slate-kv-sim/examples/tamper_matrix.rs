//! At-rest tamper / rollback rejection matrix.
//!
//! For each distinct at-rest attack this builds a real SLATE volume on the
//! file-backed path (`FileFlash` + `FileCounter`), corrupts specific bytes of
//! `data.bin` or `counter.bin`, reopens, and records exactly what the engine
//! does: which error variant `Db::open` returns, or — when the mount succeeds —
//! how many ground-truth keys still read back and whether any key returned a
//! value that differs from what was written.
//!
//! The last question is the one that matters. A rejected mount and a truncated
//! tail are both safe outcomes; returning a *wrong* value under a key is not.
//! `wrong_values` is therefore reported for every attack and must be 0
//! everywhere.
//!
//! Emits JSON on stdout. Run with:
//!   cargo run --release -p slate-kv-sim --example tamper_matrix
//!
//! Wall time is dominated by the epoch rolls: advancing an epoch requires
//! THETA appends, and three of the attacks need a volume with more than one
//! sealed epoch.

use slate_kv::file_flash::Durability;
use slate_kv::{Db, DbError, KeySource, Options, Profile};
use slate_kv_core::checkpoint::CKPT_HDR_LEN;
use slate_kv_core::config::{
    ckpt_slot_addr, data_base_offset, ERASED_BYTE, MAGIC_CKPT, MAGIC_CM, MAGIC_REC, MAGIC_XOR,
    REC_HDR_LEN, REC_OVERHEAD, THETA,
};
use slate_kv_core::record::RecordHeader;
use std::path::{Path, PathBuf};

/// Flash geometry used for every volume in this run.
const CAPACITY: u32 = 8 * 1024 * 1024;
const PAGE: usize = 256;
const BLOCK: usize = 4096;
/// Ground-truth records written below the attack surface.
const N_KEYS: usize = 48;
/// Commit batch size; small so the log holds several independent batches.
const B_COMMIT: u32 = 8;

fn opts() -> Options {
    Options {
        capacity: CAPACITY,
        b_commit: B_COMMIT,
        auto_b: false,
        // Large enough that the scheduler never commits on a staleness deadline,
        // so batch boundaries are a function of B_COMMIT alone and the log
        // layout is reproducible.
        staleness_budget_ms: 1_000_000,
        n_keys: 4096,
        profile: Profile::Pi,
        durability: Durability::OsCache,
        ..Default::default()
    }
}

fn key_of(i: usize) -> String {
    format!("gt/key/{i:04}")
}
fn val_of(i: usize) -> String {
    format!("value-{i:04}-{}", "d".repeat(1 + i % 11))
}

fn tmpbase() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "slate_tamper_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A pristine volume plus the facts a verifier needs about it.
struct Volume {
    dir: PathBuf,
    /// Epoch reported by the engine after the build finished.
    epoch: u64,
    /// `write_offset` recorded in the newest valid checkpoint: tail replay
    /// starts here, so records below it are covered by the checkpointed index
    /// and records above it are not.
    ckpt_write_offset: u32,
}

/// Builds a volume: `N_KEYS` ground-truth records, then `rolls` epoch seals
/// driven by THETA filler appends each, then a few more ground-truth-visible
/// records so the post-checkpoint tail is non-empty.
fn build_volume(base: &Path, tag: &str, rolls: usize) -> Volume {
    let dir = base.join(tag);
    let db = Db::open(&dir, KeySource::Bytes([5u8; 32]), opts()).expect("genesis open");
    for i in 0..N_KEYS {
        db.put(key_of(i).as_bytes(), val_of(i).as_bytes()).unwrap();
    }
    db.commit().unwrap();

    for _ in 0..rolls {
        let before = db.epoch();
        // The Theta trigger counts appends, not distinct keys, so a small
        // cycling key set advances the epoch without a Theta-sized index.
        for i in 0..(THETA + 256) {
            db.put(format!("fill/{:04}", i % 512).as_bytes(), b"f")
                .unwrap();
        }
        db.commit().unwrap();
        assert!(
            db.epoch() > before,
            "epoch failed to advance (still {before}); the Theta trigger did not fire"
        );
        // Re-assert the ground truth above the new checkpoint so the tail has
        // records of the current epoch in it.
        for i in 0..N_KEYS {
            db.put(key_of(i).as_bytes(), val_of(i).as_bytes()).unwrap();
        }
        db.commit().unwrap();
    }

    let epoch = db.epoch();
    drop(db);

    let mem = read_flash(&dir);
    let ckpt_write_offset = active_ckpt(&mem)
        .expect("a valid checkpoint must exist after build")
        .2;
    Volume {
        dir,
        epoch,
        ckpt_write_offset,
    }
}

fn flash_path(dir: &Path) -> PathBuf {
    dir.join("data.bin")
}
fn counter_path(dir: &Path) -> PathBuf {
    dir.join("counter.bin")
}
fn read_flash(dir: &Path) -> Vec<u8> {
    std::fs::read(flash_path(dir)).unwrap()
}
fn write_flash(dir: &Path, mem: &[u8]) {
    std::fs::write(flash_path(dir), mem).unwrap()
}

/// Copies a pristine volume so each attack works on its own bytes.
fn clone_volume(v: &Volume, tag: &str) -> PathBuf {
    let dst = v.dir.parent().unwrap().join(tag);
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::copy(flash_path(&v.dir), flash_path(&dst)).unwrap();
    std::fs::copy(counter_path(&v.dir), counter_path(&dst)).unwrap();
    dst
}

/// Frames found on flash by replicating the recovery scanner's dispatch.
///
/// The XOR parity page is skipped rather than recorded: none of the attacks
/// below target it, because a corrupt parity page is only ever read by the
/// repair path, not by mount.
#[derive(Debug, Clone, Copy)]
enum Frame {
    /// A sealed record: absolute offset, total framed length, key length.
    Record {
        off: usize,
        total: usize,
        klen: usize,
    },
    /// A commit marker pair: `off` is copy 1, `off + PAGE` is copy 2.
    CommitMarker { off: usize },
}

/// Walks the log exactly as `slate_kv_core::recover::recover` does, so the
/// offsets this returns are the ones the engine will visit.
fn scan_frames(mem: &[u8]) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut off = data_base_offset(BLOCK) as usize;
    while off < mem.len() {
        match mem[off] {
            ERASED_BYTE => {
                let rem = off % PAGE;
                if rem != 0 {
                    off += PAGE - rem;
                } else {
                    break;
                }
            }
            MAGIC_CM => {
                out.push(Frame::CommitMarker { off });
                off += PAGE * 2;
            }
            MAGIC_XOR => {
                let rem = off % PAGE;
                off += if rem != 0 { PAGE - rem } else { PAGE };
            }
            MAGIC_REC => {
                let mut hdr = [0u8; REC_HDR_LEN];
                hdr.copy_from_slice(&mem[off..off + REC_HDR_LEN]);
                match RecordHeader::decode(&hdr) {
                    Ok(h) => {
                        let total = REC_OVERHEAD + h.klen as usize + h.vlen as usize;
                        out.push(Frame::Record {
                            off,
                            total,
                            klen: h.klen as usize,
                        });
                        off += total;
                    }
                    Err(_) => break,
                }
            }
            _ => break,
        }
    }
    out
}

fn records(mem: &[u8]) -> Vec<Frame> {
    scan_frames(mem)
        .into_iter()
        .filter(|f| matches!(f, Frame::Record { .. }))
        .collect()
}
fn markers(mem: &[u8]) -> Vec<Frame> {
    scan_frames(mem)
        .into_iter()
        .filter(|f| matches!(f, Frame::CommitMarker { .. }))
        .collect()
}

/// Returns `(slot, epoch, write_offset)` of the checkpoint slot with the highest
/// epoch whose header decodes — the one `mount` will select.
fn active_ckpt(mem: &[u8]) -> Option<(u8, u64, u32)> {
    let mut best: Option<(u8, u64, u32)> = None;
    for slot in 0..2u8 {
        let a = ckpt_slot_addr(slot, BLOCK) as usize;
        if mem[a] != MAGIC_CKPT {
            continue;
        }
        let epoch = u64::from_le_bytes(mem[a + 2..a + 10].try_into().unwrap());
        let wo = u32::from_le_bytes(mem[a + 26..a + 30].try_into().unwrap());
        if best.is_none() || epoch > best.unwrap().1 {
            best = Some((slot, epoch, wo));
        }
    }
    best
}

/// Slots whose first byte is not the erased byte, i.e. slots that hold data.
fn populated_slots(mem: &[u8]) -> Vec<u8> {
    (0..2u8)
        .filter(|&s| mem[ckpt_slot_addr(s, BLOCK) as usize] != ERASED_BYTE)
        .collect()
}

/// What the engine observably did when the corrupted volume was reopened.
struct Observation {
    /// Error variant path, or `"mounted"`.
    mount_outcome: String,
    /// Ground-truth keys that read back (`None` when the mount failed).
    keys_present: Option<usize>,
    /// Keys that returned a value differing from what was written. Must be 0.
    wrong_values: Option<usize>,
    /// `acked_seq` after recovery: the highest sequence number the replay
    /// accepted. This localises where the log was truncated, which key counts
    /// cannot — a key can still read back from the checkpointed index after its
    /// tail copy was discarded.
    acked_seq: Option<u64>,
    /// Live keys in the recovered index, checkpoint plus accepted tail.
    index_len: Option<usize>,
}

fn describe_err(e: &DbError) -> String {
    match e {
        DbError::Mount(m) => format!("Err(DbError::Mount(MountError::{m:?}))"),
        DbError::Core(c) => format!("Err(DbError::Core(Error::{c:?}))"),
        DbError::Io(_) => "Err(DbError::Io)".to_string(),
        DbError::Config(_) => "Err(DbError::Config)".to_string(),
        DbError::InvalidArg(_) => "Err(DbError::InvalidArg)".to_string(),
    }
}

/// Reopens the volume and reads every ground-truth key back.
fn observe(dir: &Path) -> Observation {
    match Db::open(dir, KeySource::Bytes([5u8; 32]), opts()) {
        Err(e) => Observation {
            mount_outcome: describe_err(&e),
            keys_present: None,
            wrong_values: None,
            acked_seq: None,
            index_len: None,
        },
        Ok(db) => {
            let mut present = 0usize;
            let mut wrong = 0usize;
            for i in 0..N_KEYS {
                match db.get(key_of(i).as_bytes()) {
                    Ok(Some(v)) => {
                        present += 1;
                        if v != val_of(i).as_bytes() {
                            wrong += 1;
                        }
                    }
                    Ok(None) => {}
                    // A per-key error is neither a value nor an absence; count
                    // it as absent and let `mount_outcome` stay "mounted".
                    Err(_) => {}
                }
            }
            let mode = format!("{:?}", db.security_mode());
            let acked = db.acked_seq();
            let ilen = db.len();
            drop(db);
            Observation {
                mount_outcome: format!("Ok(mounted, security_mode={mode})"),
                keys_present: Some(present),
                wrong_values: Some(wrong),
                acked_seq: Some(acked),
                index_len: Some(ilen),
            }
        }
    }
}

/// One row of the matrix.
struct Row {
    attack: String,
    corrupted: String,
    bytes_changed: usize,
    obs: Observation,
    safe: bool,
    rationale: String,
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

impl Row {
    fn to_json(&self) -> String {
        let kp = match self.obs.keys_present {
            Some(n) => n.to_string(),
            None => "null".to_string(),
        };
        let wv = match self.obs.wrong_values {
            Some(n) => n.to_string(),
            None => "null".to_string(),
        };
        let ak = match self.obs.acked_seq {
            Some(n) => n.to_string(),
            None => "null".to_string(),
        };
        let il = match self.obs.index_len {
            Some(n) => n.to_string(),
            None => "null".to_string(),
        };
        format!(
            "    {{\n      \"attack\": \"{}\",\n      \"corrupted\": \"{}\",\n      \
             \"bytes_changed\": {},\n      \"outcome\": \"{}\",\n      \
             \"ground_truth_keys_total\": {},\n      \"ground_truth_keys_readable\": {},\n      \
             \"wrong_values_returned\": {},\n      \"acked_seq_after_recovery\": {},\n      \
             \"live_keys_in_recovered_index\": {},\n      \"safe\": {},\n      \
             \"rationale\": \"{}\"\n    }}",
            json_escape(&self.attack),
            json_escape(&self.corrupted),
            self.bytes_changed,
            json_escape(&self.obs.mount_outcome),
            N_KEYS,
            kp,
            wv,
            ak,
            il,
            self.safe,
            json_escape(&self.rationale),
        )
    }
}

/// An outcome is safe when the engine either refused the mount or came up
/// without ever handing back a value that differs from what was written.
/// Losing records is an availability cost, not a correctness failure; returning
/// a forged one is.
fn safe_from(obs: &Observation) -> bool {
    match obs.wrong_values {
        None => true,
        Some(n) => n == 0,
    }
}

fn run_attack(
    rows: &mut Vec<Row>,
    v: &Volume,
    attack: &str,
    corrupted_desc: impl FnOnce(&[u8]) -> (String, usize, Vec<u8>),
    rationale: &str,
) {
    let dir = clone_volume(v, attack);
    let mem = read_flash(&dir);
    let before = mem.clone();
    let (desc, n_changed, after) = corrupted_desc(&mem);
    assert_ne!(
        before, after,
        "attack '{attack}' did not change any flash byte"
    );
    write_flash(&dir, &after);
    let obs = observe(&dir);
    let safe = safe_from(&obs);
    rows.push(Row {
        attack: attack.to_string(),
        corrupted: desc,
        bytes_changed: n_changed,
        obs,
        safe,
        rationale: rationale.to_string(),
    });
}

fn main() {
    let base = tmpbase();
    eprintln!("workdir: {}", base.display());

    // --- volumes -----------------------------------------------------------
    let t0 = std::time::Instant::now();
    let v1 = build_volume(&base, "vol_single_epoch", 0);
    eprintln!(
        "built vol_single_epoch epoch={} ckpt_wo={} in {:.1}s",
        v1.epoch,
        v1.ckpt_write_offset,
        t0.elapsed().as_secs_f64()
    );

    // Snapshot of the one-roll volume taken BEFORE its roll, for the replay
    // attack: same device key, same counter file, older epoch image.
    let pre_roll_dir = base.join("vol_pre_roll_image");
    std::fs::create_dir_all(&pre_roll_dir).unwrap();
    std::fs::copy(flash_path(&v1.dir), flash_path(&pre_roll_dir)).unwrap();

    let t1 = std::time::Instant::now();
    let v2 = build_volume(&base, "vol_two_epochs", 1);
    eprintln!(
        "built vol_two_epochs epoch={} ckpt_wo={} in {:.1}s",
        v2.epoch,
        v2.ckpt_write_offset,
        t1.elapsed().as_secs_f64()
    );
    let t2 = std::time::Instant::now();
    let v3 = build_volume(&base, "vol_three_epochs", 2);
    eprintln!(
        "built vol_three_epochs epoch={} ckpt_wo={} in {:.1}s",
        v3.epoch,
        v3.ckpt_write_offset,
        t2.elapsed().as_secs_f64()
    );

    let mem1 = read_flash(&v1.dir);
    let recs1 = records(&mem1);
    let cms1 = markers(&mem1);
    assert!(
        recs1.len() >= N_KEYS && cms1.len() >= 2,
        "single-epoch volume must hold >= {N_KEYS} records and >= 2 commit markers, \
         found {} records and {} markers",
        recs1.len(),
        cms1.len()
    );
    assert_eq!(
        populated_slots(&read_flash(&v2.dir)).len(),
        2,
        "two-epoch volume must have both checkpoint slots populated"
    );

    let mut rows: Vec<Row> = Vec::new();

    // --- controls ----------------------------------------------------------
    // One per volume, because the attacks are spread across three volumes and a
    // reader comparing `acked_seq_after_recovery` needs the un-attacked value
    // for the same volume to compare against.
    for (v, tag) in [
        (&v1, "control_no_attack_single_epoch"),
        (&v2, "control_no_attack_two_epochs"),
        (&v3, "control_no_attack_three_epochs"),
    ] {
        let dir = clone_volume(v, tag);
        let obs = observe(&dir);
        assert_eq!(
            obs.keys_present,
            Some(N_KEYS),
            "pristine control {tag} did not return all {N_KEYS} ground-truth keys; \
             the harness cannot attribute later losses to the attacks"
        );
        rows.push(Row {
            attack: tag.into(),
            corrupted: "nothing (pristine volume)".into(),
            bytes_changed: 0,
            obs,
            safe: true,
            rationale: "Baseline: establishes that all ground-truth keys are readable, and fixes \
                        the un-attacked acked_seq for this volume, so key losses and truncation \
                        points in the attack rows are attributable to the attack."
                .into(),
        });
    }

    // --- 1. record ciphertext body ----------------------------------------
    // Two positions, because the blast radius of a detected record differs by
    // where the record sits: the scan truncates at the first record that fails
    // to open, so everything above it in the replayed tail is discarded too.
    for (label, pick) in [("first", 0usize), ("last", recs1.len() - 1)] {
        let Frame::Record { off, total, klen } = recs1[pick] else {
            unreachable!()
        };
        let target = off + REC_HDR_LEN; // first byte of the sealed body
        run_attack(
            &mut rows,
            &v1,
            &format!("record_ciphertext_body_bitflip_{label}"),
            move |mem| {
                let mut m = mem.to_vec();
                m[target] ^= 0x01;
                (
                    format!(
                        "1 bit at flash offset {target} (byte 0 of the ChaCha20-Poly1305 body of \
                         the {label} record on flash, which starts at {off}, total framed length \
                         {total}, klen {klen})"
                    ),
                    1,
                    m,
                )
            },
            "The record header is AEAD associated data and the body is the ciphertext, so a \
             flipped body bit fails Poly1305 verification. `open_record` returns Error::Tampered \
             and the replay scanner truncates the log at that record rather than applying it.",
        );
    }

    // --- 2. record header (AEAD associated data) --------------------------
    {
        let Frame::Record { off, .. } = recs1[recs1.len() - 1] else {
            unreachable!()
        };
        let target = off + 1; // low byte of `seq`, inside the AD, not the nonce
        run_attack(
            &mut rows,
            &v1,
            "record_header_bitflip",
            move |mem| {
                let mut m = mem.to_vec();
                m[target] ^= 0x01;
                (
                    format!(
                        "1 bit at flash offset {target} (low byte of the `seq` field in the \
                         28-byte record header at {off}; the header is the AEAD associated data)"
                    ),
                    1,
                    m,
                )
            },
            "The whole 28-byte header is passed as AEAD associated data, so mutating a header \
             field that the codec still accepts (seq stays in range, magic and op untouched) is \
             caught by the tag rather than by the parser.",
        );
    }

    // --- 3. AEAD tag ------------------------------------------------------
    {
        let Frame::Record { off, total, .. } = recs1[recs1.len() - 1] else {
            unreachable!()
        };
        let target = off + total - 1; // last byte of the 16-byte Poly1305 tag
        run_attack(
            &mut rows,
            &v1,
            "record_aead_tag_bitflip",
            move |mem| {
                let mut m = mem.to_vec();
                m[target] ^= 0x80;
                (
                    format!(
                        "1 bit at flash offset {target} (last byte of the 16-byte Poly1305 tag of \
                         the record at {off})"
                    ),
                    1,
                    m,
                )
            },
            "Forging the tag alone must also fail: verification compares the recomputed tag to \
             the stored one, so corrupting the stored copy is indistinguishable from corrupting \
             the body.",
        );
    }

    // --- 4. torn tail: truncate mid-record --------------------------------
    {
        let Frame::Record { off, total, .. } = recs1[recs1.len() - 1] else {
            unreachable!()
        };
        let cut = off + total / 2;
        run_attack(
            &mut rows,
            &v1,
            "log_truncated_mid_record",
            move |mem| {
                let mut m = mem.to_vec();
                let n = m.len() - cut;
                // 0xFF is the erased state, so this is exactly what a power cut
                // part-way through programming the record would leave behind.
                for b in m[cut..].iter_mut() {
                    *b = ERASED_BYTE;
                }
                (
                    format!(
                        "flash offsets {cut}..{} set to the erased byte 0xFF, cutting the record \
                         at {off} (length {total}) in half",
                        m.len()
                    ),
                    n,
                    m,
                )
            },
            "A torn tail is the ordinary power-loss case: the truncated record has no valid \
             commit marker above it, so the scan stops and the batch is never applied. This is \
             the prefix-durability property the crash Monte-Carlo also exercises.",
        );
    }

    // --- 5. commit marker -------------------------------------------------
    // Three variants, because the twin-marker scheme's failure modes differ.
    {
        let Frame::CommitMarker { off } = cms1[cms1.len() - 1] else {
            unreachable!()
        };
        let target = off + 5; // inside seq_max, magic byte left intact
        run_attack(
            &mut rows,
            &v1,
            "commit_marker_copy1_body_bitflip",
            move |mem| {
                let mut m = mem.to_vec();
                m[target] ^= 0x01;
                (
                    format!(
                        "1 bit at flash offset {target} (inside `seq_max` of commit-marker copy 1 \
                         at {off}; its 0x5C magic byte and copy 2 at {} are untouched)",
                        off + PAGE
                    ),
                    1,
                    m,
                )
            },
            "This is the case the redundant marker pair exists for: copy 1 fails its HMAC-SHA256 \
             check, the scanner falls through to copy 2, and the batch is applied normally.",
        );

        run_attack(
            &mut rows,
            &v1,
            "commit_marker_copy1_zeroed",
            move |mem| {
                let mut m = mem.to_vec();
                for b in m[off..off + PAGE].iter_mut() {
                    *b = 0x00;
                }
                (
                    format!(
                        "256 bytes at flash offsets {off}..{} zeroed (all of commit-marker copy 1, \
                         magic byte included); copy 2 at {} left valid",
                        off + PAGE,
                        off + PAGE
                    ),
                    PAGE,
                    m,
                )
            },
            "Zeroing the leading magic byte is not the same as corrupting the marker body: the \
             scanner dispatches on the first byte at the offset, so it never reaches the \
             verify-then-fall-back-to-copy-2 path and stops at that offset instead.",
        );

        run_attack(
            &mut rows,
            &v1,
            "commit_marker_both_copies_zeroed",
            move |mem| {
                let mut m = mem.to_vec();
                for b in m[off..off + 2 * PAGE].iter_mut() {
                    *b = 0x00;
                }
                (
                    format!(
                        "512 bytes at flash offsets {off}..{} zeroed (both commit-marker copies of \
                         the last batch)",
                        off + 2 * PAGE
                    ),
                    2 * PAGE,
                    m,
                )
            },
            "With no readable marker the batch it covers is unacknowledged by definition, so \
             discarding it is the correct outcome, not a loss of durable data.",
        );
    }

    // --- 6. checkpoint slots ----------------------------------------------
    // Run against the two-epoch volume so both slots hold real checkpoints and
    // the fallback path is actually reachable.
    {
        let mem2 = read_flash(&v2.dir);
        let (active_slot, active_epoch, _) = active_ckpt(&mem2).unwrap();
        let older_slot = 1 - active_slot;
        let a_active = ckpt_slot_addr(active_slot, BLOCK) as usize;
        let a_older = ckpt_slot_addr(older_slot, BLOCK) as usize;
        let t_active = a_active + CKPT_HDR_LEN;
        let t_older = a_older + CKPT_HDR_LEN;

        run_attack(
            &mut rows,
            &v2,
            "checkpoint_active_slot_bitflip",
            move |mem| {
                let mut m = mem.to_vec();
                m[t_active] ^= 0x01;
                (
                    format!(
                        "1 bit at flash offset {t_active} (first sealed byte after the 76-byte \
                         header of checkpoint slot {active_slot} at {a_active}, the newest \
                         checkpoint, epoch {active_epoch}); slot {older_slot} at {a_older} left \
                         valid"
                    ),
                    1,
                    m,
                )
            },
            "Measures whether the two-slot checkpoint scheme can silently fall back to the older \
             slot. It cannot: the older slot carries a lower epoch than the monotonic counter, so \
             the O(1) freshness-tip check rejects it. Redundancy covers a crash inside the seal \
             window, not deliberate corruption of the newest slot.",
        );

        run_attack(
            &mut rows,
            &v2,
            "checkpoint_older_slot_bitflip",
            move |mem| {
                let mut m = mem.to_vec();
                m[t_older] ^= 0x01;
                (
                    format!(
                        "1 bit at flash offset {t_older} (sealed body of checkpoint slot \
                         {older_slot} at {a_older}, the superseded checkpoint); the newest slot \
                         {active_slot} is untouched"
                    ),
                    1,
                    m,
                )
            },
            "Corrupting the superseded slot must be a no-op: `mount` picks the highest epoch \
             among slots that pass their AEAD check, and that is still the newest slot.",
        );

        run_attack(
            &mut rows,
            &v2,
            "checkpoint_both_slots_bitflip",
            move |mem| {
                let mut m = mem.to_vec();
                m[t_active] ^= 0x01;
                m[t_older] ^= 0x01;
                (
                    format!(
                        "1 bit in each checkpoint slot's sealed body (offsets {t_active} and \
                         {t_older}); no slot passes its AEAD check"
                    ),
                    2,
                    m,
                )
            },
            "With no slot authenticating and at least one slot non-erased, `mount` must refuse \
             rather than fall back to formatting a fresh volume — formatting would silently \
             discard every durable record.",
        );
    }

    // --- 7. rollback: replay an older epoch image -------------------------
    {
        let dir = base.join("rollback_replay_older_image");
        std::fs::create_dir_all(&dir).unwrap();
        // Older image (checkpoint epoch as of before the roll) + the CURRENT
        // counter file, which the roll advanced. This is the classic at-rest
        // rollback: restore yesterday's volume, the freshness tip is ahead.
        std::fs::copy(flash_path(&pre_roll_dir), flash_path(&dir)).unwrap();
        std::fs::copy(counter_path(&v2.dir), counter_path(&dir)).unwrap();
        let old_epoch = active_ckpt(&read_flash(&dir)).unwrap().1;
        let cur_epoch = active_ckpt(&read_flash(&v2.dir)).unwrap().1;
        assert!(
            cur_epoch > old_epoch,
            "rollback setup is degenerate: replayed image epoch {old_epoch} is not older than \
             the current {cur_epoch}"
        );
        let obs = observe(&dir);
        let safe = safe_from(&obs);
        rows.push(Row {
            attack: "rollback_replay_older_epoch_image".into(),
            corrupted: format!(
                "whole data.bin replaced by an earlier, internally consistent image of the same \
                 volume (checkpoint epoch {old_epoch}); counter.bin left at the current value \
                 (checkpoint epoch {cur_epoch} was sealed against it)"
            ),
            bytes_changed: CAPACITY as usize,
            obs,
            safe,
            rationale: "Every byte of the replayed image is authentic, so no AEAD check can \
                        detect it. Only the monotonic counter can: the boot rule accepts a \
                        checkpoint epoch m only when m is the counter value or one above it, and \
                        a rolled-back image has m below it."
                .into(),
        });
    }

    // --- 8. forward-spliced image (epoch gap above the counter) ------------
    {
        let dir = base.join("forward_spliced_image");
        std::fs::create_dir_all(&dir).unwrap();
        // A LATER image of the same device paired with an EARLIER counter file:
        // the mirror image of the rollback, exercising the m > MC*+1 guard.
        std::fs::copy(flash_path(&v3.dir), flash_path(&dir)).unwrap();
        std::fs::copy(counter_path(&v1.dir), counter_path(&dir)).unwrap();
        let img_epoch = active_ckpt(&read_flash(&dir)).unwrap().1;
        let ctr_epoch = active_ckpt(&read_flash(&v1.dir)).unwrap().1;
        assert!(
            img_epoch > ctr_epoch + 1,
            "forward-splice setup is degenerate: image epoch {img_epoch} is not more than one \
             above the counter-matched epoch {ctr_epoch}"
        );
        let obs = observe(&dir);
        let safe = safe_from(&obs);
        rows.push(Row {
            attack: "forward_spliced_image_epoch_gap".into(),
            corrupted: format!(
                "data.bin from a later image of the same device (checkpoint epoch {img_epoch}) \
                 paired with counter.bin from an earlier one (matching epoch {ctr_epoch})"
            ),
            bytes_changed: CAPACITY as usize,
            obs,
            safe,
            rationale: "The upper half of the boot rule. A gap of more than one epoch between \
                        the checkpoint and the counter cannot arise from a crash inside the seal \
                        window, so it indicates a spliced pair of authentic-but-mismatched \
                        artefacts and must be refused."
                .into(),
        });
    }

    // --- 9. cross-epoch record splice -------------------------------------
    // Copy an authentic data page from below the checkpoint (an earlier epoch)
    // over a data page in the post-checkpoint tail (the current epoch). Both
    // pages are genuine engine output sealed with genuine keys; only their
    // position is wrong.
    {
        let mem2 = read_flash(&v2.dir);
        let wo = v2.ckpt_write_offset as usize;
        let all = records(&mem2);
        let src = all
            .iter()
            .find_map(|f| match f {
                Frame::Record { off, .. } if *off < wo && off % PAGE == 0 => Some(*off),
                _ => None,
            })
            .expect("a page-aligned record below the checkpoint write offset");
        let dst = all
            .iter()
            .find_map(|f| match f {
                Frame::Record { off, .. } if *off >= wo && off % PAGE == 0 => Some(*off),
                _ => None,
            })
            .expect("a page-aligned record in the post-checkpoint tail");
        assert_ne!(src, dst);
        run_attack(
            &mut rows,
            &v2,
            "cross_epoch_record_splice",
            move |mem| {
                let mut m = mem.to_vec();
                let page: Vec<u8> = m[src..src + PAGE].to_vec();
                m[dst..dst + PAGE].copy_from_slice(&page);
                (
                    format!(
                        "256-byte page of authentic sealed records copied from flash offset {src} \
                         (below the checkpoint write offset {wo}, so sealed in an earlier epoch) \
                         over the page at {dst} (in the replayed tail of the current epoch)"
                    ),
                    PAGE,
                    m,
                )
            },
            "Splicing needs no key: the attacker relocates the engine's own authentic output. \
             The record's AEAD tag still verifies, because the epoch travels in the \
             authenticated nonce and its key is re-derivable. What fails is the batch's hash \
             chain: the commit marker commits to a chain value over the records actually written \
             there, so the batch does not match and is dropped.",
        );
    }

    // --- 10. counter file itself ------------------------------------------
    {
        let dir = clone_volume(&v1, "counter_file_hmac_corrupt");
        let mut ctr = std::fs::read(counter_path(&dir)).unwrap();
        assert_eq!(ctr.len(), 80, "counter file should be two 40-byte slots");
        // Both slots' HMAC-SHA256 tags, leaving the values readable. No key
        // needed, so this is within reach of an at-rest attacker.
        ctr[8] ^= 0x01;
        ctr[48] ^= 0x01;
        std::fs::write(counter_path(&dir), &ctr).unwrap();
        let obs = observe(&dir);
        let safe = safe_from(&obs);
        rows.push(Row {
            attack: "counter_file_hmac_corrupt_both_slots".into(),
            corrupted: "1 bit in the HMAC-SHA256 tag of each of counter.bin's two 40-byte slots \
                        (offsets 8 and 48); the stored counter values are left intact"
                .into(),
            bytes_changed: 2,
            obs,
            safe,
            rationale: "The software counter authenticates each slot with a key derived from the \
                        device key, so a value the attacker cannot re-MAC is unusable. With \
                        neither slot verifying, the freshness tip is unavailable and the mount \
                        must be refused rather than proceeding without a rollback check."
                .into(),
        });
    }

    // --- security modes on both paths -------------------------------------
    let std_mode = {
        let dir = clone_volume(&v1, "probe_security_mode_std");
        let db = Db::open(&dir, KeySource::Bytes([5u8; 32]), opts()).unwrap();
        let m = format!("{:?}", db.security_mode());
        drop(db);
        m
    };
    // The sim path is measured twice on purpose. `mount` derives the mode from
    // `CounterKind`, but the genesis-format branch (no checkpoint on flash yet)
    // constructs `EngineState` with a hardcoded mode instead, so a freshly
    // formatted volume misreports a hardware counter as best-effort. Only the
    // remount value reflects the counter.
    let (sim_kind, sim_mode_genesis, sim_mode_remount) = {
        use slate_kv_hal::MonotonicCounter;
        use slate_kv_sim::sim_db::{Db as SimDb, KeySource as SimKey, Options as SimOpts};
        use slate_kv_sim::{SimCounter, SimFlash};
        let sim_opts = SimOpts {
            capacity: 1024 * 1024,
            b_commit: B_COMMIT,
            auto_b: false,
            staleness_budget_ms: 1_000_000,
            n_keys: 1024,
            profile: slate_kv_sim::sim_db::Profile::Pi,
        };
        let flash = SimFlash::new(sim_opts.capacity, PAGE, BLOCK);
        let counter = SimCounter::new(100_000);
        let kind = format!("{:?}", MonotonicCounter::kind(&counter));
        let mut db = SimDb::open(SimKey::Bytes([5u8; 32]), sim_opts.clone(), flash, counter)
            .expect("sim genesis open");
        let genesis = format!("{:?}", db.security_mode());
        for i in 0..24 {
            db.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
        }
        db.commit().unwrap();
        let (flash, counter) = db.take_flash_and_counter();
        let db2 =
            SimDb::open(SimKey::Bytes([5u8; 32]), sim_opts, flash, counter).expect("sim remount");
        let remount = format!("{:?}", db2.security_mode());
        drop(db2);
        (kind, genesis, remount)
    };

    // --- emit --------------------------------------------------------------
    let n_unsafe = rows.iter().filter(|r| !r.safe).count();
    let n_wrong: usize = rows.iter().filter_map(|r| r.obs.wrong_values).sum();

    println!("{{");
    println!("  \"provenance\": {{");
    println!("    \"command\": \"cargo run --release -p slate-kv-sim --example tamper_matrix\",");
    println!(
        "    \"platform\": \"macOS 26.5.2 / arm64 / 12 cores / 24 GiB; file-backed flash \
         emulation (FileFlash) plus one in-RAM SimFlash probe. NOT a Raspberry Pi, NOT an \
         ESP32.\","
    );
    println!(
        "    \"geometry\": \"capacity {CAPACITY} B, page {PAGE} B, erase block {BLOCK} B, \
         data_base_offset {} , checkpoint slot 0 at {}, slot 1 at {}, THETA {THETA}, b_commit \
         {B_COMMIT}, durability OsCache\",",
        data_base_offset(BLOCK),
        ckpt_slot_addr(0, BLOCK),
        ckpt_slot_addr(1, BLOCK)
    );
    println!(
        "    \"ground_truth\": \"{N_KEYS} distinct key/value records written and committed on \
         every volume before the attack; each row reports how many still read back and how many \
         returned a value differing from what was written\","
    );
    println!(
        "    \"volumes\": \"vol_single_epoch: engine epoch {} after build, checkpoint \
         write_offset {}. vol_two_epochs: epoch {}, write_offset {}. vol_three_epochs: epoch {}, \
         write_offset {}.\"",
        v1.epoch,
        v1.ckpt_write_offset,
        v2.epoch,
        v2.ckpt_write_offset,
        v3.epoch,
        v3.ckpt_write_offset
    );
    println!("  }},");
    println!("  \"security_mode\": {{");
    println!(
        "    \"std_file_counter_path_on_remount\": \"{}\",",
        json_escape(&std_mode)
    );
    println!("    \"std_file_counter_kind\": \"BestEffort\",");
    println!("    \"sim_counter_kind\": \"{}\",", json_escape(&sim_kind));
    println!(
        "    \"sim_counter_path_on_genesis_format\": \"{}\",",
        json_escape(&sim_mode_genesis)
    );
    println!(
        "    \"sim_counter_path_on_remount\": \"{}\",",
        json_escape(&sim_mode_remount)
    );
    println!(
        "    \"note\": \"FileCounter reports CounterKind::BestEffort, so the std path reports \
         SecurityMode::BestEffortRollback — the honest G3 degradation: a counter file on a \
         general-purpose filesystem can be snapshotted and restored together with the volume, so \
         its monotonicity is only as strong as the surrounding system. A hardware monotonic \
         counter (eFuse, RPMC-backed flash, or a TEE-held counter) declares \
         CounterKind::Hardware and yields SecurityMode::Full, measured here via SimCounter. The \
         boot rule executed is byte-identical in both modes: only the strength of the freshness \
         tip differs. CounterKind::None would give NoRollbackProtection and skip the tip check \
         entirely; no path in this run reported it.\",",
    );
    println!(
        "    \"finding_genesis_mode_misreport\": \"The mode reported immediately after a \
         genesis format does NOT reflect the counter: `mount`'s FormatError branch constructs \
         EngineState with a hardcoded BestEffortRollback, so a Hardware counter is reported as \
         BestEffortRollback until the first remount, where it correctly becomes Full. This is a \
         reporting defect only — a freshly formatted volume has no prior epoch to roll back to, \
         so no rollback check is skipped — but any figure quoting security_mode must state \
         whether it was read on genesis or on remount.\"",
    );
    println!("  }},");
    println!("  \"summary\": {{");
    println!("    \"n_attacks\": {},", rows.len());
    println!("    \"n_unsafe_outcomes\": {n_unsafe},");
    println!("    \"n_wrong_values_returned_total\": {n_wrong}");
    println!("  }},");
    println!("  \"attacks\": [");
    let n = rows.len();
    for (i, r) in rows.iter().enumerate() {
        print!("{}", r.to_json());
        println!("{}", if i + 1 == n { "" } else { "," });
    }
    println!("  ]");
    println!("}}");

    let _ = std::fs::remove_dir_all(&base);
}
