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
    pub fn new(base: u32, len: u32, flash: esp_hal::peripherals::FLASH<'a>) -> Self {
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

    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        if addr + buf.len() as u32 > self.len {
            return Err(EspFlashError::OutOfBounds);
        }
        self.inner
            .read(self.base + addr, buf)
            .map_err(|_| EspFlashError::StorageError)
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
/// region so the two never write the same sector; the demo binaries map the log
/// at `[0x100000, 0x180000)`, well below this address. Only referenced by the
/// `counter-efuse` persistence path.
#[cfg(feature = "counter-efuse")]
const COUNTER_FLASH_ADDR: u32 = 0x300000;

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
