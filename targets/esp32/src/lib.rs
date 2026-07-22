#![no_std]

use slate_hal::{Flash, CounterKind, MonotonicCounter};
use esp_storage::FlashStorage;
use embedded_storage::nor_flash::{ReadNorFlash, NorFlash};

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
        Self { base, len, inner: FlashStorage::new(flash) }
    }
}

impl<'a> Flash for EspFlash<'a> {
    type Error = EspFlashError;

    fn page_size(&self) -> usize { 256 }
    fn block_size(&self) -> usize { 4096 }
    fn capacity(&self) -> u32 { self.len }

    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        if addr + buf.len() as u32 > self.len { return Err(EspFlashError::OutOfBounds); }
        self.inner.read(self.base + addr, buf).map_err(|_| EspFlashError::StorageError)
    }

    fn program(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        if addr % 256 != 0 || buf.len() % 256 != 0 { return Err(EspFlashError::Unaligned); }
        if addr + buf.len() as u32 > self.len { return Err(EspFlashError::OutOfBounds); }
        self.inner.write(self.base + addr, buf).map_err(|_| EspFlashError::StorageError)
    }

    fn erase(&mut self, block_addr: u32) -> Result<(), Self::Error> {
        if block_addr % 4096 != 0 { return Err(EspFlashError::Unaligned); }
        if block_addr + 4096 > self.len { return Err(EspFlashError::OutOfBounds); }
        self.inner.erase(self.base + block_addr, self.base + block_addr + 4096).map_err(|_| EspFlashError::StorageError)
    }
}

#[derive(Debug)]
pub enum EspCounterError {
    FormatError,
    Exhausted,
}

pub struct EspCounter {
    val: u64,
}

impl EspCounter {
    pub fn new() -> Self { Self { val: 0 } }
}

impl MonotonicCounter for EspCounter {
    type Error = EspCounterError;

    fn kind(&self) -> CounterKind {
        #[cfg(feature = "counter-flash")]
        return CounterKind::BestEffort;
        
        #[cfg(feature = "counter-efuse")]
        return CounterKind::Hardware;
        
        #[cfg(feature = "counter-none")]
        return CounterKind::None;
        
        #[cfg(not(any(feature = "counter-flash", feature = "counter-efuse", feature = "counter-none")))]
        return CounterKind::None;
    }

    fn read(&mut self) -> Result<u64, Self::Error> {
        Ok(self.val) // Stub for now
    }

    fn increment(&mut self) -> Result<u64, Self::Error> {
        self.val += 1;
        Ok(self.val) // Stub for now
    }
}
