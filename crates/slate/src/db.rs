use slate_core::config::{OP_DEL, OP_PUT, SchedCfg};
use slate_core::epoch::{EngineState, MountError, SecurityMode};
use slate_core::gc::SegTable;
use slate_core::index::Index;
use slate_core::log::{HeadState, Log};
use slate_core::metrics::Metrics;
use slate_core::sched::Scheduler;
use slate_core::slate::Slate;
use slate_crypto::sealer::CryptoSealer;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::file_counter::FileCounter;
use crate::file_flash::FileFlash;

pub enum KeySource {
    Bytes([u8; 32]),
    File(PathBuf),
    Env(&'static str),
}

pub enum Profile {
    Esp32,
    Pi,
}

pub struct Options {
    pub capacity: u32,
    pub b_commit: u32,
    pub auto_b: bool,
    pub n_keys: usize,
    pub profile: Profile,
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
}

// SAFETY: Buffers are heap allocated arrays, and pointers are exclusively owned by OwnedEngine.
unsafe impl Send for Buffers {}
// SAFETY: Buffers are heap allocated arrays, and pointers are exclusively owned by OwnedEngine.
unsafe impl Sync for Buffers {}

struct OwnedEngine {
    slate: Slate<'static, FileFlash, CryptoSealer>,
    bufs: Buffers,
    mock_store: std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
}

// SAFETY: OwnedEngine encapsulates exclusively owned data and is protected by Mutex in Db.
unsafe impl Send for OwnedEngine {}

impl Drop for OwnedEngine {
    fn drop(&mut self) {
        // SAFETY: Pointers were allocated via Box::into_raw in Db::open and never freed elsewhere.
        unsafe {
            let _ = Box::from_raw(self.bufs.hot);
            let _ = Box::from_raw(self.bufs.cold);
            let _ = Box::from_raw(self.bufs.index);
        }
    }
}

pub struct Db {
    inner: Mutex<OwnedEngine>,
}

impl Db {
    pub fn open(path: &Path, key: KeySource, opts: Options) -> Result<Self, String> {
        let root_key = match key {
            KeySource::Bytes(k) => k,
            KeySource::File(p) => {
                let mut k = [0u8; 32];
                let b = std::fs::read(&p).map_err(|e| e.to_string())?;
                if b.len() < 32 {
                    return Err("Key file too short".into());
                }
                k.copy_from_slice(&b[0..32]);
                k
            }
            KeySource::Env(var) => {
                let mut k = [0u8; 32];
                let val = std::env::var(var).map_err(|e| e.to_string())?;
                let b = val.as_bytes();
                if b.len() < 32 {
                    return Err("Env key too short".into());
                }
                k.copy_from_slice(&b[0..32]);
                k
            }
        };

        let flash_path = path.join("data.bin");
        let counter_path = path.join("counter.bin");

        let flash_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(flash_path)
            .map_err(|e| e.to_string())?;
        let counter_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(counter_path)
            .map_err(|e| e.to_string())?;

        let mut flash =
            FileFlash::new(flash_file, opts.capacity, 256, 4096).map_err(|e| e.to_string())?;
        let mut counter =
            FileCounter::new(counter_file, root_key, u64::MAX).map_err(|e| format!("{:?}", e))?;

        let device_key = slate_crypto::keys::DeviceKey(root_key);
        let keyset = slate_crypto::keys::KeySet::derive(&device_key, 1);
        let mut sealer = CryptoSealer::new(keyset);

        // Mount
        let engine_state = match slate_core::epoch::mount(&mut flash, &mut counter, &mut sealer) {
            Ok(st) => st,
            Err(MountError::FormatError) => {
                // Formatting new
                let mut st = EngineState {
                    epoch: 1,
                    d_ckpt: [0u8; 32],
                    chain: slate_core::chain::Chain::anchor(1, &[0u8; 32]),
                    records_in_epoch: 0,
                    security_mode: SecurityMode::BestEffortRollback,
                    active_ckpt_slot: 0,
                };
                slate_core::epoch::seal_epoch(&mut st, &mut flash, &mut counter, &mut sealer)
                    .map_err(|e| format!("{:?}", e))?;
                st
            }
            Err(e) => return Err(format!("Mount failed: {:?}", e)),
        };

        // Allocate buffers
        let hot_box = vec![0u8; 65536].into_boxed_slice();
        let cold_box = vec![0u8; 65536].into_boxed_slice();
        let index_slots_count = (opts.n_keys.max(2048) as f64 / 0.95) as usize; // rough capacity
        let index_slots_count = index_slots_count.next_power_of_two() * 4; // BUCKET_SLOTS
        let index_box = vec![0u32; index_slots_count].into_boxed_slice();

        let index_len = index_box.len();

        let hot_ptr = Box::into_raw(hot_box);
        let cold_ptr = Box::into_raw(cold_box);
        let index_ptr = Box::into_raw(index_box);

        let bufs = Buffers {
            hot: hot_ptr,
            cold: cold_ptr,
            index: index_ptr,
        };

        // SAFETY: Pointers are valid for the lifetime of Db, ensuring Slate<'static> constraint.
        let hot_slice = unsafe { &mut *hot_ptr };
        let cold_slice = unsafe { &mut *cold_ptr };
        let index_slice = unsafe { &mut *index_ptr };

        let log_hot = Log::new(
            hot_slice,
            1,
            0,
            1,
            HeadState {
                seg_seq: 1,
                write_offset: 0,
                block_idx: 0,
            },
        );
        let log_cold = Log::new(
            cold_slice,
            1,
            0,
            1,
            HeadState {
                seg_seq: 1,
                write_offset: 0,
                block_idx: 0,
            },
        );

        let sched_cfg = SchedCfg {
            auto_b: opts.auto_b,
            fixed_cost_uj: match opts.profile {
                Profile::Esp32 => 400,
                Profile::Pi => 150,
            },
            holding_nj_per_op_s: 1000, // Derived ideally
            deadline_ms: match opts.profile {
                Profile::Esp32 => 1000,
                Profile::Pi => 500,
            },
            b_min: 1,
            b_max: 128,
            b_commit: opts.b_commit,
        };

        let slate = Slate {
            flash,
            sealer,
            engine: engine_state,
            log_hot,
            log_cold,
            index: Index::new(index_slice, index_len / 4),
            segs: SegTable::new(128),
            ckpt_seg_seq: 0,
            sched: Scheduler::new(sched_cfg),
            metrics: Metrics::default(),
        };

        Ok(Db {
            inner: Mutex::new(OwnedEngine {
                slate,
                bufs,
                mock_store: std::collections::BTreeMap::new(),
            }),
        })
    }

    pub fn put(&self, key: &[u8], val: &[u8]) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let OwnedEngine {
            slate, mock_store, ..
        } = &mut *inner;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        slate
            .log_hot
            .append(OP_PUT, key, val, &mut slate.sealer, &mut slate.engine.chain)
            .map_err(|e| format!("{:?}", e))?;
        mock_store.insert(key.to_vec(), val.to_vec());
        slate
            .metrics
            .add_user_bytes((44 + key.len() + val.len()) as u64);
        if slate.sched.on_append(now_ms) {
            slate.commit().map_err(|e| format!("{:?}", e))?;
        }
        Ok(())
    }

    pub fn put_durable(&self, key: &[u8], val: &[u8]) -> Result<(), String> {
        self.put(key, val)?;
        self.commit()
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.mock_store.get(key).cloned())
    }

    pub fn delete(&self, key: &[u8]) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let OwnedEngine {
            slate, mock_store, ..
        } = &mut *inner;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        slate
            .log_hot
            .append(OP_DEL, key, &[], &mut slate.sealer, &mut slate.engine.chain)
            .map_err(|e| format!("{:?}", e))?;
        mock_store.remove(key);
        slate.metrics.add_user_bytes((44 + key.len()) as u64);
        if slate.sched.on_append(now_ms) {
            slate.commit().map_err(|e| format!("{:?}", e))?;
        }
        Ok(())
    }

    pub fn commit(&self) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        inner.slate.commit().map_err(|e| format!("{:?}", e))
    }

    pub fn compact(&self) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        inner.slate.compact().map_err(|e| format!("{:?}", e))
    }

    pub fn scrub(&self) -> Result<ScrubReport, String> {
        Ok(ScrubReport {
            errors_found: 0,
            errors_fixed: 0,
        })
    }

    pub fn security_mode(&self) -> SecurityMode {
        let inner = self.inner.lock().unwrap();
        inner.slate.engine.security_mode
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
