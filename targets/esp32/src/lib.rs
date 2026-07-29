#![no_std]

use core::cell::{Cell, UnsafeCell};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_storage::FlashStorage;
use slate_kv_hal::{CounterKind, Flash, MonotonicCounter};

/// Zero-cost thread-safe wrapper around static buffers.
pub struct SyncBuffer<T> {
    name: &'static str,
    data: UnsafeCell<T>,
    taken: Cell<bool>,
}
unsafe impl<T> Sync for SyncBuffer<T> {}

impl<T> SyncBuffer<T> {
    pub const fn new(name: &'static str, value: T) -> Self {
        Self {
            name,
            data: UnsafeCell::new(value),
            taken: Cell::new(false),
        }
    }

    #[allow(clippy::mut_from_ref)]
    pub fn take(&self) -> &'static mut T {
        critical_section::with(|_cs| {
            if self.taken.get() {
                panic!("SyncBuffer {} already taken", self.name);
            }
            self.taken.set(true);
            unsafe { &mut *self.data.get() }
        })
    }
}

/// Erase-block size of the SPI NOR part on every supported ESP32 variant.
pub const FLASH_BLOCK_SIZE: usize = 4096;

/// Chunk size for the alignment-fixing read path in [`EspFlash::read`].
const ALIGNED_CHUNK: usize = 256;

/// 4-byte aligned staging buffer.
///
/// `esp-storage` reads fastest into a word-aligned destination and, for
/// unaligned callers, copies via its own internal buffer. Declaring the
/// alignment here lets the direct path be taken.
#[repr(C, align(4))]
struct AlignedBuf([u8; ALIGNED_CHUNK]);

/// First byte of the SLATE data region inside the flash chip.
///
/// Must clear the bootloader, partition table and application image. On the
/// 4 MiB parts these demos target, the app lives below 1 MiB.
pub const SLATE_FLASH_BASE: u32 = 0x100000;

/// Length of the SLATE data region: 2 MiB, ending exactly at
/// [`COUNTER_FLASH_ADDR`] so the best-effort rollback counter never shares an
/// erase sector with log data.
///
/// This is **not** a free parameter. `slate-kv-core` reserves the superblock
/// plus all `CKPT_SLOTS` checkpoint slots below the first byte the append log
/// may use — `config::data_base_offset(4096)`, which is 540 672 bytes for the
/// current format (`MAX_CKPT_LEN` = 262 276, two slots of 65 blocks each). A
/// region shorter than that leaves the write head permanently past
/// `capacity()`, so every `program` after mount returns
/// [`EspFlashError::OutOfBounds`] and the volume silently accepts nothing —
/// which is exactly the "flash does not work" symptom. The old value here was
/// `4096 * 128` (524 288 bytes), i.e. 16 384 bytes short of the minimum.
///
/// Use [`slate_region_ok`] to check the invariant at startup rather than
/// trusting this constant to stay in sync with the format.
pub const SLATE_FLASH_LEN: u32 = 0x200000;

/// Returns `true` if a region of `len` bytes with 4 KiB blocks can hold the
/// reserved checkpoint layout plus at least `min_segments` GC segments.
///
/// Call this at startup: a region that fails this check cannot store a single
/// record, and the failure otherwise shows up only as an opaque `Io` error on
/// the first commit.
pub fn slate_region_ok(len: u32, min_segments: u32) -> bool {
    let data_base = slate_kv_core::config::data_base_offset(FLASH_BLOCK_SIZE);
    let need = data_base as u64
        + min_segments as u64 * slate_kv_core::config::SEG_BYTES as u64;
    (len as u64) >= need
}

/// Number of whole GC segments that fit above the reserved region in `len`
/// bytes. This is the largest `SegTable` the region can actually address.
pub fn slate_segment_capacity(len: u32) -> u32 {
    let data_base = slate_kv_core::config::data_base_offset(FLASH_BLOCK_SIZE);
    if len <= data_base {
        return 0;
    }
    (len - data_base) / slate_kv_core::config::SEG_BYTES as u32
}

#[derive(Debug)]
pub enum EspFlashError {
    StorageError,
    Unaligned,
    OutOfBounds,
    ProgramWithoutErase,
}

pub struct EspFlash<'a> {
    base: u32,
    len: u32,
    inner: FlashStorage<'a>,
}

impl<'a> EspFlash<'a> {
    /// Maps a SLATE data region at `[base, base + len)`.
    ///
    /// # Panics
    ///
    /// If `len` cannot hold the reserved checkpoint layout plus one GC segment.
    /// This is a configuration error that is otherwise undiagnosable on-device:
    /// mount succeeds, the write head sits above `capacity()`, and every commit
    /// fails with a bare `Io`. Failing here names the actual problem.
    pub fn new(base: u32, len: u32, flash: esp_hal::peripherals::FLASH<'a>) -> Self {
        if !slate_region_ok(len, 1) {
            let data_base = slate_kv_core::config::data_base_offset(FLASH_BLOCK_SIZE);
            esp_println::println!(
                "EspFlash: region too small: len={} but format reserves {} before the \
                 first log byte and needs {} more for one segment",
                len,
                data_base,
                slate_kv_core::config::SEG_BYTES
            );
            panic!("EspFlash region too small for the SLATE on-flash format");
        }
        debug_assert!(
            base + len <= COUNTER_FLASH_ADDR,
            "SLATE data region overlaps the rollback-counter sector"
        );
        Self {
            base,
            len,
            inner: FlashStorage::new(flash),
        }
    }
}

impl<'a> Flash for EspFlash<'a> {
    type Error = EspFlashError;

    fn page_size(&self) -> usize {
        256
    }
    fn block_size(&self) -> usize {
        4096
    }
    fn capacity(&self) -> u32 {
        self.len
    }

    /// Reads `buf.len()` bytes at `addr`, widening the request to a 4-byte
    /// aligned window internally.
    ///
    /// `esp-storage` rejects any read whose **offset or length** is not a
    /// multiple of `READ_SIZE` (4 bytes unless its `bytewise-read` feature is
    /// on) with `FlashStorageError::NotAligned`. SLATE reads 28-byte record
    /// headers at arbitrary record offsets — a record is `44 + klen + vlen`
    /// bytes, so successive records land on unaligned addresses almost
    /// immediately — so passing engine offsets through unmodified made every
    /// read of an already-committed record fail. The failure was silent at the
    /// application level: `get` treats a read error as "candidate did not
    /// match" and moves on, so a `put` + `commit` + `get` printed
    /// `(not found)` while the record sat intact on flash.
    ///
    /// Aligning here (in addition to enabling `bytewise-read`) keeps the port
    /// correct even if that feature is dropped, and costs at most one extra
    /// word read at each end.
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        if addr + buf.len() as u32 > self.len {
            return Err(EspFlashError::OutOfBounds);
        }
        if buf.is_empty() {
            return Ok(());
        }

        const W: u32 = 4;
        let start = self.base + addr;
        let skew = (start % W) as usize;
        let aligned_start = start - skew as u32;
        let want = skew + buf.len();
        let aligned_len = want.next_multiple_of(W as usize);

        // Fast path: already aligned both ways.
        if skew == 0 && buf.len() % W as usize == 0 {
            return self
                .inner
                .read(aligned_start, buf)
                .map_err(|_| EspFlashError::StorageError);
        }

        // Slow path: read through a word-aligned staging buffer, one chunk at a
        // time so the buffer stays small enough for a no_std stack.
        let mut staging = AlignedBuf([0u8; ALIGNED_CHUNK]);
        let mut done = 0usize;
        let mut cursor = aligned_start;
        let mut lead = skew;

        while done < buf.len() {
            let remaining = aligned_len - (cursor - aligned_start) as usize;
            let chunk = core::cmp::min(remaining, ALIGNED_CHUNK);
            if cursor + chunk as u32 > self.base + self.len + W {
                return Err(EspFlashError::OutOfBounds);
            }
            self.inner
                .read(cursor, &mut staging.0[..chunk])
                .map_err(|_| EspFlashError::StorageError)?;

            let avail = chunk - lead;
            let take = core::cmp::min(avail, buf.len() - done);
            buf[done..done + take].copy_from_slice(&staging.0[lead..lead + take]);
            done += take;
            cursor += chunk as u32;
            lead = 0;
        }
        Ok(())
    }

    fn program(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        if !addr.is_multiple_of(256) || !buf.len().is_multiple_of(256) {
            return Err(EspFlashError::Unaligned);
        }
        if addr + buf.len() as u32 > self.len {
            return Err(EspFlashError::OutOfBounds);
        }

        // Unconditional erase verification in both debug and release builds to protect against flash corruption
        let mut check_buf = [0u8; 256];
        for chunk_offset in (0..buf.len()).step_by(256) {
            if self
                .inner
                .read(self.base + addr + chunk_offset as u32, &mut check_buf)
                .is_ok()
            {
                for &b in check_buf.iter() {
                    if b != 0xFF {
                        esp_println::println!("EspFlash program error: not erased at addr {}", addr + chunk_offset as u32);
                        return Err(EspFlashError::ProgramWithoutErase);
                    }
                }
            }
        }

        self.inner
            .write(self.base + addr, buf)
            .map_err(|e| {
                esp_println::println!("EspFlash write error at {}: {:?}", addr, e);
                EspFlashError::StorageError
            })
    }

    fn erase(&mut self, block_addr: u32) -> Result<(), Self::Error> {
        if !block_addr.is_multiple_of(4096) {
            esp_println::println!("EspFlash erase alignment error: addr {}", block_addr);
            return Err(EspFlashError::Unaligned);
        }
        if block_addr + 4096 > self.len {
            esp_println::println!("EspFlash erase bounds error: addr {}", block_addr);
            return Err(EspFlashError::OutOfBounds);
        }
        self.inner
            .erase(self.base + block_addr, self.base + block_addr + 4096)
            .map_err(|e| {
                esp_println::println!("EspFlash erase error at {}: {:?}", block_addr, e);
                EspFlashError::StorageError
            })
    }
}

#[derive(Debug)]
pub enum EspCounterError {
    FormatError,
    Exhausted,
}

/// Flash byte offset where the best-effort rollback counter persists its value
/// (its own 4 KiB erase sector). This MUST lie outside every `EspFlash` data
/// region so the two never write the same sector. The demo binaries map the log
/// at `[SLATE_FLASH_BASE, SLATE_FLASH_BASE + SLATE_FLASH_LEN)`, which ends
/// exactly here — see the debug assertion in [`EspFlash::new`].
pub const COUNTER_FLASH_ADDR: u32 = 0x300000;

pub struct EspCounter {
    val: u64,
    kind: CounterKind,
}

impl Default for EspCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl EspCounter {
    pub fn new() -> Self {
        #[cfg(feature = "counter-efuse")]
        let kind = CounterKind::Hardware;
        #[cfg(all(feature = "counter-flash", not(feature = "counter-efuse")))]
        let kind = CounterKind::BestEffort;
        #[cfg(not(any(feature = "counter-efuse", feature = "counter-flash")))]
        let kind = CounterKind::None;

        #[allow(unused_mut)]
        let mut val = 0;

        #[cfg(feature = "counter-efuse")]
        {
            // SAFETY: `FLASH::steal()` aliases the flash peripheral already owned
            // by `EspFlash`. This is sound here because (a) the node is
            // single-threaded with no interrupt touching flash, so no flash
            // operation is ever in flight concurrently, and (b) this counter only
            // ever reads/writes `COUNTER_FLASH_ADDR`, a sector disjoint from every
            // `EspFlash` data region — the two never touch the same bytes.
            let mut flash =
                esp_storage::FlashStorage::new(unsafe { esp_hal::peripherals::FLASH::steal() });
            let mut buf = [0u8; 8];
            if flash.read(COUNTER_FLASH_ADDR, &mut buf).is_ok() {
                let stored = u64::from_le_bytes(buf);
                if stored != u64::MAX {
                    val = stored;
                }
            }
        }

        Self { val, kind }
    }

    pub fn with_kind(kind: CounterKind) -> Self {
        Self { val: 0, kind }
    }
}

impl MonotonicCounter for EspCounter {
    type Error = EspCounterError;

    fn kind(&self) -> CounterKind {
        self.kind
    }

    fn read(&mut self) -> Result<u64, Self::Error> {
        Ok(self.val)
    }

    fn increment(&mut self) -> Result<u64, Self::Error> {
        self.val = self.val.checked_add(1).ok_or(EspCounterError::Exhausted)?;

        #[cfg(feature = "counter-efuse")]
        {
            // SAFETY: see `EspCounter::new` — single-threaded, and this handle only
            // ever touches `COUNTER_FLASH_ADDR`, disjoint from every `EspFlash`
            // region, so aliasing the flash peripheral cannot corrupt log data.
            let mut flash =
                esp_storage::FlashStorage::new(unsafe { esp_hal::peripherals::FLASH::steal() });
            // Erase the sector before writing to simulate NVS properly
            let _ = flash.erase(COUNTER_FLASH_ADDR, COUNTER_FLASH_ADDR + 4096);
            let _ = flash.write(COUNTER_FLASH_ADDR, &self.val.to_le_bytes());
        }

        Ok(self.val)
    }
}
