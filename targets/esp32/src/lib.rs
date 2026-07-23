#![no_std]

use core::cell::UnsafeCell;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_storage::FlashStorage;
use slate_hal::{CounterKind, Flash, MonotonicCounter};

/// Zero-cost thread-safe wrapper around static buffers to eliminate static mut UB.
pub struct SyncBuffer<T>(UnsafeCell<T>);
unsafe impl<T> Sync for SyncBuffer<T> {}

impl<T> SyncBuffer<T> {
    pub const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    #[allow(clippy::mut_from_ref)]
    pub fn as_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
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
                        return Err(EspFlashError::ProgramWithoutErase);
                    }
                }
            }
        }

        self.inner
            .write(self.base + addr, buf)
            .map_err(|_| EspFlashError::StorageError)
    }

    fn erase(&mut self, block_addr: u32) -> Result<(), Self::Error> {
        if !block_addr.is_multiple_of(4096) {
            return Err(EspFlashError::Unaligned);
        }
        if block_addr + 4096 > self.len {
            return Err(EspFlashError::OutOfBounds);
        }
        self.inner
            .erase(self.base + block_addr, self.base + block_addr + 4096)
            .map_err(|_| EspFlashError::StorageError)
    }
}

#[derive(Debug)]
pub enum EspCounterError {
    FormatError,
    Exhausted,
}

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

        Self { val: 0, kind }
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
        Ok(self.val)
    }
}

