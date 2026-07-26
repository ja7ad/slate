use slate_kv_core::config::{SchedCfg, OP_DEL, OP_PUT};
use slate_kv_core::epoch::{EngineState, MountError, SecurityMode};
use slate_kv_core::gc::SegTable;
use slate_kv_core::index::Index;
use slate_kv_core::log::{HeadState, Log};
use slate_kv_core::metrics::Metrics;
use slate_kv_core::sched::Scheduler;
use slate_kv_core::slate::Slate;
use slate_kv_crypto::sealer::CryptoSealer;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::file_counter::FileCounter;
use crate::file_flash::FileFlash;

#[derive(Debug)]
pub enum DbError {
    Core(slate_kv_core::error::Error),
    Mount(slate_kv_core::epoch::MountError),
    Io(std::io::Error),
    Config(String),
    InvalidArg(String),
}

impl From<slate_kv_core::error::Error> for DbError {
    fn from(e: slate_kv_core::error::Error) -> Self {
        DbError::Core(e)
    }
}
impl From<slate_kv_core::epoch::MountError> for DbError {
    fn from(e: slate_kv_core::epoch::MountError) -> Self {
        DbError::Mount(e)
    }
}
impl From<std::io::Error> for DbError {
    fn from(e: std::io::Error) -> Self {
        DbError::Io(e)
    }
}
impl From<String> for DbError {
    fn from(e: String) -> Self {
        DbError::Config(e)
    }
}
impl From<&str> for DbError {
    fn from(e: &str) -> Self {
        DbError::Config(e.to_string())
    }
}

pub enum KeySource {
    Bytes([u8; 32]),
    File(PathBuf),
    Env(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Esp32,
    Pi,
}

/// Tuning knobs for [`Db::open`].
///
/// `Clone` so a caller can open the same configuration twice — reopening a
/// database after a drop is the normal way to test durability, and without
/// `Clone` every such call site has to rebuild the struct field by field.
#[derive(Clone, Debug)]
pub struct Options {
    pub capacity: u32,
    pub b_commit: u32,
    pub auto_b: bool,
    pub staleness_budget_ms: u32,
    pub n_keys: usize,
    pub profile: Profile,
    pub durability: crate::file_flash::Durability,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            capacity: 4 * 1024 * 1024,
            b_commit: 8,
            auto_b: true,
            staleness_budget_ms: 1000,
            n_keys: 8192,
            profile: Profile::Pi,
            durability: crate::file_flash::Durability::Full,
        }
    }
}

pub struct ScrubReport {
    pub errors_found: u32,
    pub errors_fixed: u32,
}

// Opaque stats struct for passing metrics
#[derive(Default, Clone)]
pub struct Stats {
    pub commits: u64,
    pub wakes: u64,
    pub user_bytes: u64,
    pub gc_bytes: u64,
    pub parity_bytes: u64,
    pub ckpt_bytes: u64,
    pub erases: u64,
}

// Box pointers to free on drop
struct Buffers {
    hot: *mut [u8],
    cold: *mut [u8],
    index: *mut [u32],
    ckpt: *mut [u8],
}

// SAFETY: Buffers are heap allocated arrays, and pointers are exclusively owned by Db/OwnedEngine.
unsafe impl Send for Buffers {}
unsafe impl Sync for Buffers {}

impl Drop for Buffers {
    fn drop(&mut self) {
        // SAFETY: Pointers were allocated via Box::into_raw in Db::open and never freed elsewhere.
        unsafe {
            let _ = Box::from_raw(self.hot);
            let _ = Box::from_raw(self.cold);
            let _ = Box::from_raw(self.index);
            let _ = Box::from_raw(self.ckpt);
        }
    }
}

struct OwnedEngine {
    slate: Slate<'static, FileFlash, FileCounter, CryptoSealer>,
    #[allow(dead_code)]
    bufs: Buffers, // Dropped after slate because of declaration order
}

// SAFETY: OwnedEngine encapsulates exclusively owned data and is protected by Mutex in Db.
unsafe impl Send for OwnedEngine {}

pub struct Db {
    inner: Mutex<OwnedEngine>,
}

impl Drop for Db {
    /// Best-effort flush of the pending batch on a clean close.
    ///
    /// Records sit in the in-memory batch until a commit marker is written, so
    /// without this a `drop` after a successful `put` loses up to `b_max`
    /// records — a surprising result for callers who never crashed. This is a
    /// convenience, not a durability guarantee: the error is deliberately
    /// swallowed because `drop` cannot report one, so any caller that must know
    /// its data is durable has to call [`Db::commit`] and check the result.
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            let _ = inner.slate.commit();
        }
    }
}

impl Db {
    #[allow(clippy::collapsible_if)]
    pub fn open(path: &Path, key: KeySource, opts: Options) -> Result<Self, DbError> {
        let root_key = match key {
            KeySource::Bytes(k) => k,
            KeySource::File(p) => {
                let mut k = [0u8; 32];
                let b = std::fs::read(&p)?;
                if b.len() < 32 {
                    return Err(DbError::Config("Key file too short".into()));
                }
                k.copy_from_slice(&b[0..32]);
                k
            }
            KeySource::Env(var) => {
                let mut k = [0u8; 32];
                let val = std::env::var(var).map_err(|e| DbError::Config(e.to_string()))?;
                let b = val.as_bytes();
                if b.len() < 32 {
                    return Err(DbError::Config("Env key too short".into()));
                }
                k.copy_from_slice(&b[0..32]);
                k
            }
        };

        use slate_kv_core::log::Sealer;
        std::fs::create_dir_all(path)?;

        let flash_path = path.join("data.bin");
        let counter_path = path.join("counter.bin");

        let flash_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(flash_path)?;
        let counter_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(counter_path)?;

        let mut flash = FileFlash::new(flash_file, opts.capacity, 256, 4096, opts.durability)
            .map_err(|e| DbError::Config(e.to_string()))?;
        let device_key = slate_kv_crypto::keys::DeviceKey(root_key);
        let keyset = slate_kv_crypto::keys::KeySet::derive(&device_key, 1);
        let k_ctr = keyset.k_ctr;

        let mut counter = FileCounter::new(counter_file, k_ctr, u64::MAX).map_err(|e| match e {
            // FormatError is only ever returned when a counter slot fails HMAC
            // verification, i.e. it is a tamper signal, not a config problem —
            // it must stay distinguishable from operational errors (RULES.md).
            crate::file_counter::FileCounterError::FormatError => {
                DbError::Mount(MountError::Tampered)
            }
            crate::file_counter::FileCounterError::Io(err) => DbError::Io(err),
            crate::file_counter::FileCounterError::Exhausted => {
                DbError::Config("monotonic counter budget exhausted".to_string())
            }
        })?;

        let mut sealer = CryptoSealer::new(keyset);

        // Allocate buffers
        let hot_box = vec![0u8; 65536].into_boxed_slice();
        let cold_box = vec![0u8; 65536].into_boxed_slice();
        // Size the cuckoo table for the requested key count, then reject up
        // front anything whose serialized form would not fit in a checkpoint
        // slot. The whole index goes into one checkpoint, so exceeding
        // MAX_CKPT_LEN does not degrade gracefully: every epoch seal would fail
        // and the database would run without checkpoints entirely.
        let n_buckets = (opts.n_keys.max(2048) as f64 / 0.95) as usize;
        let index_slots_count = n_buckets.next_power_of_two() * slate_kv_core::config::BUCKET_SLOTS;
        let max_slots = slate_kv_core::config::max_index_slots();
        if index_slots_count > max_slots {
            return Err(DbError::Config(format!(
                "n_keys={} needs {} index slots but the checkpoint format allows at most {} \
                 (MAX_CKPT_LEN={}); reduce n_keys to at most {}",
                opts.n_keys,
                index_slots_count,
                max_slots,
                slate_kv_core::config::MAX_CKPT_LEN,
                max_slots / slate_kv_core::config::BUCKET_SLOTS * 95 / 100,
            )));
        }
        let index_box = vec![0u32; index_slots_count].into_boxed_slice();
        let index_len = index_box.len();

        // The checkpoint buffer must hold header + serialized index + AEAD tag.
        // Sizing it from the same helper the format uses keeps the two in step;
        // the previous `index_len + 1024` under-counted because the index costs
        // 4 bytes per slot, not 1.
        let ckpt_box =
            vec![0u8; slate_kv_core::config::ckpt_len_for_slots(index_len)].into_boxed_slice();
        let ckpt_ptr = Box::into_raw(ckpt_box);
        let ckpt_slice = unsafe { &mut *ckpt_ptr };

        // Mount. `replay_from` is where tail replay begins: the log head as of
        // the newest checkpoint, or the base of the log for a freshly formatted
        // volume. Everything below it is already captured in the checkpointed
        // index.
        let data_base = slate_kv_core::config::data_base_offset(4096);
        let (engine_state, plain_len, replay_from, ckpt_seg_seq) =
            match slate_kv_core::epoch::mount(&mut flash, &mut counter, &mut sealer, ckpt_slice) {
                Ok(mi) => (
                    mi.state,
                    mi.plain_len,
                    mi.ckpt_write_offset.max(data_base),
                    mi.ckpt_seg_seq,
                ),
                Err(MountError::FormatError) => {
                    // Formatting new
                    let mut st = EngineState {
                        epoch: 1,
                        next_seq: 1,
                        acked_seq: 0,
                        d_ckpt: [0u8; 32],
                        chain: slate_kv_core::chain::Chain::anchor(1, &[0u8; 32]),
                        records_in_epoch: 0,
                        security_mode: SecurityMode::BestEffortRollback,
                        active_ckpt_slot: 0,
                    };
                    // The genesis checkpoint records the head at `data_base`,
                    // where the log actually begins. Recording 0 here would make
                    // the next mount replay the reserved checkpoint region as if
                    // it were log data.
                    slate_kv_core::epoch::seal_epoch(
                        &mut st,
                        &mut flash,
                        &mut counter,
                        &mut sealer,
                        1,
                        data_base,
                        0,
                        ckpt_slice,
                        0,
                    )?;
                    (st, 0, data_base, 1)
                }
                Err(e) => return Err(e.into()),
            };

        sealer.roll_epoch(engine_state.epoch);

        let hot_ptr = Box::into_raw(hot_box);
        let cold_ptr = Box::into_raw(cold_box);
        let index_ptr = Box::into_raw(index_box);

        let bufs = Buffers {
            hot: hot_ptr,
            cold: cold_ptr,
            index: index_ptr,
            ckpt: ckpt_ptr,
        };

        // SAFETY: Pointers are valid for the lifetime of Db, ensuring Slate<'static> constraint.
        let hot_slice = unsafe { &mut *hot_ptr };
        let cold_slice = unsafe { &mut *cold_ptr };
        let index_slice = unsafe { &mut *index_ptr };

        // Both logs start above the reserved checkpoint region. Starting at 0
        // would put the first records on top of the live checkpoint slots: the
        // hot head is repositioned by recovery below, but the cold head is not,
        // so a cold write would program still-live checkpoint pages.
        let data_base = slate_kv_core::config::data_base_offset(4096);
        let log_hot = Log::new(
            hot_slice,
            HeadState {
                seg_seq: 1,
                write_offset: data_base,
                block_idx: 0,
            },
        );
        let log_cold = Log::new(
            cold_slice,
            HeadState {
                seg_seq: 1,
                write_offset: data_base,
                block_idx: 0,
            },
        );

        let sched_cfg = SchedCfg {
            auto_b: opts.auto_b,
            fixed_cost_uj: match opts.profile {
                Profile::Esp32 => 400,
                Profile::Pi => 150,
            },
            staleness_budget_ms: opts.staleness_budget_ms,
            deadline_ms: match opts.profile {
                Profile::Esp32 => 1000,
                Profile::Pi => 500,
            },
            b_min: 1,
            b_max: 128,
            b_commit: opts.b_commit,
        };

        let rng_seed = engine_state.epoch.max(1) ^ 42;
        let mut slate = Slate {
            flash,
            counter,
            sealer,
            engine: engine_state,
            log_hot,
            log_cold,
            index: Index::new(index_slice, index_len / 4),
            segs: SegTable::new(128),
            // Seeded from the checkpoint so GC's reclaim watermark survives a
            // remount; starting at 0 would block victim selection until the
            // next epoch seal.
            ckpt_seg_seq,
            sched: Scheduler::new(sched_cfg),
            metrics: Metrics::default(),
            ckpt_buf: ckpt_slice,
            rng: slate_kv_core::index::XorShift64::new(rng_seed),
        };

        if plain_len > 0 {
            slate.index.deserialize(
                &slate.ckpt_buf[slate_kv_core::checkpoint::CKPT_HDR_LEN
                    ..slate_kv_core::checkpoint::CKPT_HDR_LEN + plain_len],
            );
        }

        let mut index_upsert_error = false;
        let mut rng = slate_kv_core::index::XorShift64::new(42);
        let mut workspace = Box::new(slate_kv_core::recover::RecoverWorkspace::new());
        // Replay only the tail: records written after the newest checkpoint.
        // The checkpointed index already accounts for everything below
        // `replay_from`, so rescanning from the base of the log would both cost
        // O(log length) and fail to validate — those older batches' commit
        // markers belong to earlier epochs.
        let rec_info = slate_kv_core::recover::recover(
            &mut slate.flash,
            &mut slate.sealer,
            &mut slate.engine.chain,
            slate.engine.epoch,
            replay_from,
            &mut workspace,
            |flash, sealer, _seq, off, op, key| {
                // Replay in seq order, deduplicating by the full key so repeated
                // Puts/Deletes to the same key collapse to a single live slot
                // rather than filling the table with superseded entries.
                if op == slate_kv_core::config::OP_PUT {
                    let res = slate.index.upsert(key, off, &mut rng, |cand_off| {
                        slate_kv_core::recover::record_key_eq(flash, sealer, cand_off, key)
                    });
                    if res.is_err() {
                        index_upsert_error = true;
                    }
                } else if op == slate_kv_core::config::OP_DEL {
                    slate.index.remove(key, |cand_off| {
                        slate_kv_core::recover::record_key_eq(flash, sealer, cand_off, key)
                    });
                }
            },
        )
        .map_err(|e| DbError::Config(format!("{:?}", e)))?;

        // Never let the head land below where the checkpoint said the log
        // already reached: an empty tail returns `head_pos == replay_from`, and
        // anything lower would overwrite durable records.
        slate.log_hot.head.write_offset = rec_info.head_pos.max(replay_from);
        slate.log_cold.head.write_offset = slate.log_hot.head.write_offset;
        // `mount` seeded next_seq from the checkpoint (= ckpt.seq). Never let the
        // tail replay regress the sequence counter below it — a crash immediately
        // after a checkpoint leaves committed_upto == 0 with a non-trivial
        // checkpoint seq, and reusing those seq numbers would break AEAD nonce
        // uniqueness and the total order.
        let mount_next = slate.engine.next_seq;
        slate.engine.next_seq = (rec_info.committed_upto + 1).max(mount_next);
        slate.engine.acked_seq = rec_info.committed_upto.max(mount_next.saturating_sub(1));

        if index_upsert_error {
            return Err("Index capacity exceeded during recovery".into());
        }

        Ok(Db {
            inner: Mutex::new(OwnedEngine { slate, bufs }),
        })
    }

    pub fn put(&self, key: &[u8], val: &[u8]) -> Result<(), DbError> {
        if key.len() > slate_kv_core::config::MAX_KEY_LEN
            || val.len() > slate_kv_core::config::MAX_VAL_LEN
        {
            return Err(DbError::InvalidArg("key or value too large".into()));
        }
        let mut inner = self.inner.lock().unwrap();
        let OwnedEngine { slate, .. } = &mut *inner;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let offset = slate.append_hot(OP_PUT, key, val)?;
        slate.index_update_offset(key, offset)?;
        slate
            .metrics
            .add_user_bytes((slate_kv_core::config::REC_OVERHEAD + key.len() + val.len()) as u64);
        if slate.sched.on_append(now_ms) {
            slate.commit()?;
        }
        Ok(())
    }

    pub fn put_durable(&self, key: &[u8], val: &[u8]) -> Result<(), DbError> {
        if key.len() > slate_kv_core::config::MAX_KEY_LEN
            || val.len() > slate_kv_core::config::MAX_VAL_LEN
        {
            return Err(DbError::InvalidArg("key or value too large".into()));
        }
        self.put(key, val)?;
        self.commit()
    }

    #[allow(clippy::collapsible_if)]
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, DbError> {
        if key.len() > slate_kv_core::config::MAX_KEY_LEN {
            return Err(DbError::InvalidArg("key too large".into()));
        }
        let mut inner = self.inner.lock().unwrap();
        let mut cbuf = slate_kv_core::index::CandidateBuf::new();
        inner.slate.index.candidates(key, &mut cbuf);

        let mut best_val: Option<Vec<u8>> = None;
        let mut best_seq = 0;

        for &off in cbuf.as_slice() {
            let mut rec_bytes = None;

            // Check hot log batch
            let hot_write_off = inner.slate.log_hot.head.write_offset;
            let hot_data = inner.slate.log_hot.batch.data();
            if off >= hot_write_off && off < hot_write_off + hot_data.len() as u32 {
                let rec_off = (off - hot_write_off) as usize;
                if rec_off + slate_kv_core::config::REC_HDR_LEN <= hot_data.len() {
                    let mut hdr_bytes = [0u8; slate_kv_core::config::REC_HDR_LEN];
                    hdr_bytes.copy_from_slice(
                        &hot_data[rec_off..rec_off + slate_kv_core::config::REC_HDR_LEN],
                    );
                    if let Ok(hdr) = slate_kv_core::record::RecordHeader::decode(&hdr_bytes) {
                        let total_len = slate_kv_core::config::REC_OVERHEAD
                            + hdr.klen as usize
                            + hdr.vlen as usize;
                        if rec_off + total_len <= hot_data.len() {
                            rec_bytes =
                                Some((hdr_bytes, hot_data[rec_off..rec_off + total_len].to_vec()));
                        }
                    }
                }
            }

            // Check cold log batch
            let cold_write_off = inner.slate.log_cold.head.write_offset;
            let cold_data = inner.slate.log_cold.batch.data();
            if rec_bytes.is_none()
                && off >= cold_write_off
                && off < cold_write_off + cold_data.len() as u32
            {
                let rec_off = (off - cold_write_off) as usize;
                if rec_off + slate_kv_core::config::REC_HDR_LEN <= cold_data.len() {
                    let mut hdr_bytes = [0u8; slate_kv_core::config::REC_HDR_LEN];
                    hdr_bytes.copy_from_slice(
                        &cold_data[rec_off..rec_off + slate_kv_core::config::REC_HDR_LEN],
                    );
                    if let Ok(hdr) = slate_kv_core::record::RecordHeader::decode(&hdr_bytes) {
                        let total_len = slate_kv_core::config::REC_OVERHEAD
                            + hdr.klen as usize
                            + hdr.vlen as usize;
                        if rec_off + total_len <= cold_data.len() {
                            rec_bytes =
                                Some((hdr_bytes, cold_data[rec_off..rec_off + total_len].to_vec()));
                        }
                    }
                }
            }

            // Read from flash if not in batch
            if rec_bytes.is_none() {
                use slate_kv_hal::Flash;
                let mut hdr_bytes = [0u8; slate_kv_core::config::REC_HDR_LEN];
                if inner.slate.flash.read(off, &mut hdr_bytes).is_ok() {
                    if let Ok(hdr) = slate_kv_core::record::RecordHeader::decode(&hdr_bytes) {
                        let total_len = slate_kv_core::config::REC_OVERHEAD
                            + hdr.klen as usize
                            + hdr.vlen as usize;
                        let mut rb = vec![0u8; total_len];
                        if inner.slate.flash.read(off, &mut rb).is_ok() {
                            rec_bytes = Some((hdr_bytes, rb));
                        }
                    }
                }
            }

            if let Some((hdr_bytes, rb)) = rec_bytes {
                if let Ok(hdr) = slate_kv_core::record::RecordHeader::decode(&hdr_bytes) {
                    let mut plain_out = vec![0u8; rb.len() - 16];
                    use slate_kv_core::log::Sealer;
                    if inner
                        .slate
                        .sealer
                        .open_record(
                            &hdr_bytes,
                            &rb[slate_kv_core::config::REC_HDR_LEN..],
                            &mut plain_out,
                        )
                        .is_ok()
                    {
                        if plain_out[..hdr.klen as usize] == *key {
                            if hdr.seq >= best_seq {
                                best_seq = hdr.seq;
                                if hdr.op == slate_kv_core::config::OP_PUT {
                                    let val_start = hdr.klen as usize;
                                    best_val = Some(
                                        plain_out[val_start..val_start + hdr.vlen as usize]
                                            .to_vec(),
                                    );
                                } else {
                                    best_val = None;
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(best_val)
    }

    pub fn delete_durable(&self, key: &[u8]) -> Result<(), DbError> {
        if key.len() > slate_kv_core::config::MAX_KEY_LEN {
            return Err(DbError::InvalidArg("key too large".into()));
        }
        let mut inner = self.inner.lock().unwrap();
        let OwnedEngine { slate, .. } = &mut *inner;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let _ = slate.append_hot(OP_DEL, key, &[])?;

        slate.index_remove_key(key);
        slate
            .metrics
            .add_user_bytes((slate_kv_core::config::REC_OVERHEAD + key.len()) as u64);
        if slate.sched.on_append(now_ms) {
            slate.commit()?;
        }
        Ok(())
    }

    pub fn delete(&self, key: &[u8]) -> Result<(), DbError> {
        if key.len() > slate_kv_core::config::MAX_KEY_LEN {
            return Err(DbError::InvalidArg("key too large".into()));
        }
        let mut inner = self.inner.lock().unwrap();
        let OwnedEngine { slate, .. } = &mut *inner;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let _ = slate.append_hot(OP_DEL, key, &[])?;

        slate.index_remove_key(key);
        slate
            .metrics
            .add_user_bytes((slate_kv_core::config::REC_OVERHEAD + key.len()) as u64);
        if slate.sched.on_append(now_ms) {
            slate.commit()?;
        }
        Ok(())
    }

    pub fn commit(&self) -> Result<(), DbError> {
        let mut inner = self.inner.lock().unwrap();
        inner.slate.commit()?;
        Ok(())
    }

    pub fn compact(&self) -> Result<(), DbError> {
        let mut inner = self.inner.lock().unwrap();
        inner.slate.compact()?;
        Ok(())
    }

    pub fn scrub(&self) -> Result<ScrubReport, DbError> {
        Ok(ScrubReport {
            errors_found: 0,
            errors_fixed: 0,
        })
    }

    pub fn security_mode(&self) -> SecurityMode {
        let inner = self.inner.lock().unwrap();
        inner.slate.engine.security_mode
    }

    pub fn acked_seq(&self) -> u64 {
        self.inner.lock().unwrap().slate.engine.acked_seq
    }

    pub fn next_seq(&self) -> u64 {
        self.inner.lock().unwrap().slate.engine.next_seq
    }

    pub fn epoch(&self) -> u64 {
        self.inner.lock().unwrap().slate.engine.epoch
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().slate.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn stats(&self) -> Stats {
        let inner = self.inner.lock().unwrap();
        let m = &inner.slate.metrics;
        Stats {
            commits: m.commits,
            wakes: m.wakes,
            user_bytes: m.user_bytes,
            gc_bytes: m.gc_bytes,
            parity_bytes: m.parity_bytes,
            ckpt_bytes: m.ckpt_bytes,
            erases: m.erases,
        }
    }
}
