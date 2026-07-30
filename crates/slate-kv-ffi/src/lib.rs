#![allow(non_camel_case_types)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
use slate_kv::{Db, KeySource, Options, Profile};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::catch_unwind;
use std::path::Path;
use std::sync::Mutex;

pub const SLATE_OK: i32 = 0;
pub const SLATE_ERR_NOT_FOUND: i32 = -1;
pub const SLATE_ERR_BUFFER_TOO_SMALL: i32 = -2;
pub const SLATE_ERR_INVALID_ARG: i32 = -3;
pub const SLATE_ERR_TAMPERED: i32 = -10;
pub const SLATE_ERR_ROLLBACK: i32 = -11;
pub const SLATE_ERR_INTERNAL: i32 = -99;
pub const SLATE_ERR_IO: i32 = -100;

pub const SLATE_ABI_VERSION_MAJOR: u16 = 1;
pub const SLATE_ABI_VERSION_MINOR: u16 = 0;

/// `slate_options::profile` selector: Raspberry-Pi-class host (report §9: B = 9).
pub const SLATE_PROFILE_PI: u8 = 0;
/// `slate_options::profile` selector: ESP32-class device (report §9: B = 27).
pub const SLATE_PROFILE_ESP32: u8 = 1;

#[no_mangle]
pub extern "C" fn slate_abi_version() -> u32 {
    ((SLATE_ABI_VERSION_MAJOR as u32) << 16) | (SLATE_ABI_VERSION_MINOR as u32)
}

#[repr(C)]
pub struct slate_options {
    pub capacity_bytes: u64,
    pub max_keys: u32,
    pub b_commit: u32,
    pub theta: u32,
    pub profile: u8, // 0 = Pi, 1 = Esp32
}

/// Message from the most recent failed [`slate_open`].
///
/// A failed open returns no handle, so this is the only channel through which
/// the reason can reach the caller. It is a process-wide `Mutex`, not a
/// `thread_local!`, for two reasons:
///
///  * **Correctness across threads.** A C caller may open on one thread and
///    read the message on another; with thread-local storage that caller
///    silently got an empty string. Nothing in the C ABI promises thread
///    affinity, so the storage must not assume it.
///  * **Portability.** `thread_local!` lowers to a `#[thread_local]` static,
///    whose TLS model is fixed by the libc the static library was built
///    against. Linking a glibc-built `staticlib` into a musl-based host (as
///    TinyGo does) then faults on the first access — and `slate_open` clears
///    this on its *success* path, so even open-then-close crashed.
///
/// Concurrent failing opens on different threads can overwrite each other's
/// message. SLATE is single-writer by design, so that is accepted rather than
/// paid for with per-thread storage.
static LAST_OPEN_ERROR: Mutex<String> = Mutex::new(String::new());

fn set_open_error(msg: String) {
    if let Ok(mut e) = LAST_OPEN_ERROR.lock() {
        *e = msg;
    }
}

pub struct slate_db {
    db: Db,
    last_error: Mutex<String>,
}

fn set_error(db: *mut slate_db, msg: String) {
    if !db.is_null() {
        unsafe {
            if let Ok(mut lock) = (*db).last_error.lock() {
                *lock = msg;
            }
        }
    }
}

fn map_err(e: &slate_kv::DbError) -> i32 {
    match e {
        slate_kv::DbError::Core(slate_kv_core::error::Error::Tampered) => SLATE_ERR_TAMPERED,
        slate_kv::DbError::Core(slate_kv_core::error::Error::Rollback) => SLATE_ERR_ROLLBACK,
        slate_kv::DbError::Mount(slate_kv_core::epoch::MountError::Tampered) => SLATE_ERR_TAMPERED,
        slate_kv::DbError::Mount(slate_kv_core::epoch::MountError::Rollback) => SLATE_ERR_ROLLBACK,
        slate_kv::DbError::Mount(slate_kv_core::epoch::MountError::Io) => SLATE_ERR_IO,
        slate_kv::DbError::Io(_) => SLATE_ERR_IO,
        slate_kv::DbError::InvalidArg(_) => SLATE_ERR_INVALID_ARG,
        _ => SLATE_ERR_INTERNAL,
    }
}

macro_rules! catch_ffi {
    ($db:expr, $body:expr) => {
        match catch_unwind(|| $body) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                let code = map_err(&e);
                set_error($db, format!("{:?}", e));
                code
            }
            Err(_) => SLATE_ERR_INTERNAL,
        }
    };
}

#[no_mangle]
pub extern "C" fn slate_open(
    path: *const c_char,
    key: *const u8,
    opts: *const slate_options,
    out: *mut *mut slate_db,
) -> i32 {
    if path.is_null() || key.is_null() || opts.is_null() || out.is_null() {
        set_open_error("null pointer passed to slate_open".to_string());
        return SLATE_ERR_INVALID_ARG;
    }
    unsafe { *out = std::ptr::null_mut() };

    match catch_unwind(|| {
        let path_c = unsafe { CStr::from_ptr(path) };
        let path_str = match path_c.to_str() {
            Ok(s) => s,
            Err(_) => {
                let err_msg = "Invalid UTF-8 in path".to_string();
                set_open_error(err_msg.clone());
                return Err(slate_kv::DbError::Config(err_msg));
            }
        };

        let mut root_key = [0u8; 32];
        unsafe {
            root_key.copy_from_slice(std::slice::from_raw_parts(key, 32));
        }

        let opts_ref = unsafe { &*opts };
        if opts_ref.capacity_bytes > u32::MAX as u64 {
            let err_msg = "capacity_bytes exceeds u32::MAX".to_string();
            set_open_error(err_msg.clone());
            return Err(slate_kv::DbError::InvalidArg(err_msg));
        }

        let rs_opts = Options {
            capacity: opts_ref.capacity_bytes as u32,
            b_commit: opts_ref.b_commit,
            auto_b: opts_ref.b_commit == 0,
            staleness_budget_ms: 1000,
            n_keys: opts_ref.max_keys as usize,
            profile: if opts_ref.profile == SLATE_PROFILE_PI {
                Profile::Pi
            } else {
                Profile::Esp32
            },
            durability: slate_kv::file_flash::Durability::Full,
        };

        let db = match Db::open(Path::new(path_str), KeySource::Bytes(root_key), rs_opts) {
            Ok(d) => d,
            Err(e) => {
                set_open_error(format!("{:?}", e));
                return Err(e);
            }
        };

        let slate_db_ptr = Box::into_raw(Box::new(slate_db {
            db,
            last_error: Mutex::new(String::new()),
        }));

        set_open_error(String::new());
        unsafe { *out = slate_db_ptr };
        Ok::<i32, slate_kv::DbError>(SLATE_OK)
    }) {
        Ok(Ok(code)) => code,
        Ok(Err(e)) => map_err(&e),
        Err(_) => {
            set_open_error("internal panic during open".to_string());
            SLATE_ERR_INTERNAL
        }
    }
}

#[no_mangle]
pub extern "C" fn slate_put(
    db: *mut slate_db,
    k: *const u8,
    klen: usize,
    v: *const u8,
    vlen: usize,
) -> i32 {
    if db.is_null() || k.is_null() || klen == 0 || (v.is_null() && vlen > 0) {
        return SLATE_ERR_INVALID_ARG;
    }
    catch_ffi!(db, {
        let key_slice = unsafe { std::slice::from_raw_parts(k, klen) };
        let val_slice = if vlen == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(v, vlen) }
        };
        unsafe { (*db).db.put(key_slice, val_slice) }.map(|_| SLATE_OK)
    })
}

#[no_mangle]
pub extern "C" fn slate_put_durable(
    db: *mut slate_db,
    k: *const u8,
    klen: usize,
    v: *const u8,
    vlen: usize,
) -> i32 {
    if db.is_null() || k.is_null() || klen == 0 || (v.is_null() && vlen > 0) {
        return SLATE_ERR_INVALID_ARG;
    }
    catch_ffi!(db, {
        let key_slice = unsafe { std::slice::from_raw_parts(k, klen) };
        let val_slice = if vlen == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(v, vlen) }
        };
        unsafe { (*db).db.put_durable(key_slice, val_slice) }.map(|_| SLATE_OK)
    })
}

#[no_mangle]
pub extern "C" fn slate_get(
    db: *mut slate_db,
    k: *const u8,
    klen: usize,
    v_out: *mut u8,
    vlen_inout: *mut usize,
) -> i32 {
    if db.is_null() || k.is_null() || klen == 0 || vlen_inout.is_null() {
        return SLATE_ERR_INVALID_ARG;
    }

    catch_ffi!(db, {
        let key_slice = unsafe { std::slice::from_raw_parts(k, klen) };
        let val_opt = match unsafe { (*db).db.get(key_slice) } {
            Ok(v) => v,
            Err(e) => return Err(e),
        };

        match val_opt {
            Some(val) => {
                let capacity = unsafe { *vlen_inout };
                unsafe { *vlen_inout = val.len() };

                if capacity < val.len() {
                    Ok::<i32, slate_kv::DbError>(SLATE_ERR_BUFFER_TOO_SMALL)
                } else {
                    if !v_out.is_null() && !val.is_empty() {
                        unsafe {
                            std::ptr::copy_nonoverlapping(val.as_ptr(), v_out, val.len());
                        }
                    }
                    Ok::<i32, slate_kv::DbError>(SLATE_OK)
                }
            }
            None => Ok::<i32, slate_kv::DbError>(SLATE_ERR_NOT_FOUND),
        }
    })
}

#[no_mangle]
pub extern "C" fn slate_delete(db: *mut slate_db, k: *const u8, klen: usize) -> i32 {
    if db.is_null() || k.is_null() || klen == 0 {
        return SLATE_ERR_INVALID_ARG;
    }
    catch_ffi!(db, {
        let key_slice = unsafe { std::slice::from_raw_parts(k, klen) };
        unsafe { (*db).db.delete(key_slice) }.map(|_| SLATE_OK)
    })
}

#[no_mangle]
pub extern "C" fn slate_commit(db: *mut slate_db) -> i32 {
    if db.is_null() {
        return SLATE_ERR_INVALID_ARG;
    }
    catch_ffi!(db, { unsafe { (*db).db.commit() }.map(|_| SLATE_OK) })
}

#[no_mangle]
pub extern "C" fn slate_compact(db: *mut slate_db) -> i32 {
    if db.is_null() {
        return SLATE_ERR_INVALID_ARG;
    }
    catch_ffi!(db, { unsafe { (*db).db.compact() }.map(|_| SLATE_OK) })
}

#[no_mangle]
pub extern "C" fn slate_security_mode(db: *mut slate_db) -> i32 {
    if db.is_null() {
        return -1;
    }
    match catch_unwind(|| unsafe { (*db).db.security_mode() }) {
        Ok(slate_kv_core::epoch::SecurityMode::Full) => 0,
        Ok(slate_kv_core::epoch::SecurityMode::BestEffortRollback) => 1,
        Ok(slate_kv_core::epoch::SecurityMode::NoRollbackProtection) => 2,
        Err(_) => -1,
    }
}

#[no_mangle]
pub extern "C" fn slate_close(db: *mut slate_db) -> i32 {
    if db.is_null() {
        return SLATE_ERR_INVALID_ARG;
    }
    match catch_unwind(|| {
        unsafe {
            let _ = (*db).db.commit(); // Best effort
            let _ = Box::from_raw(db);
        }
        SLATE_OK
    }) {
        Ok(c) => c,
        Err(_) => SLATE_ERR_INTERNAL,
    }
}

#[no_mangle]
pub extern "C" fn slate_last_error_message(
    db: *mut slate_db,
    buf: *mut c_char,
    len: usize,
) -> usize {
    if buf.is_null() || len == 0 {
        return 0;
    }
    let msg = match catch_unwind(|| {
        if db.is_null() {
            LAST_OPEN_ERROR
                .lock()
                .map(|e| e.clone())
                .unwrap_or_default()
        } else {
            unsafe {
                if let Ok(lock) = (*db).last_error.lock() {
                    lock.clone()
                } else {
                    String::new()
                }
            }
        }
    }) {
        Ok(s) => s,
        Err(_) => return 0,
    };

    let bytes = msg.as_bytes();
    let copy_len = std::cmp::min(bytes.len(), len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, copy_len);
        *(buf as *mut u8).add(copy_len) = 0; // null terminator
    }
    copy_len
}
