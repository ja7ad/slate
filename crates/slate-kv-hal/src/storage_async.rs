//! Adapter for `embedded-storage-async` implementations.

use crate::AsyncFlash;
use embedded_storage_async::nor_flash::NorFlash;

/// Adapts any [`NorFlash`] from `embedded-storage-async` to implement [`AsyncFlash`].
pub struct NorFlashAdapter<T>(pub T);

impl<T: NorFlash> AsyncFlash for NorFlashAdapter<T> {
    type Error = T::Error;

    fn page_size(&self) -> usize {
        T::WRITE_SIZE
    }

    fn block_size(&self) -> usize {
        T::ERASE_SIZE
    }

    fn capacity(&self) -> u32 {
        self.0.capacity() as u32
    }

    fn read(
        &mut self,
        addr: u32,
        buf: &mut [u8],
    ) -> impl core::future::Future<Output = Result<(), Self::Error>> {
        self.0.read(addr, buf)
    }

    fn program(
        &mut self,
        addr: u32,
        buf: &[u8],
    ) -> impl core::future::Future<Output = Result<(), Self::Error>> {
        self.0.write(addr, buf)
    }

    fn erase(
        &mut self,
        block_addr: u32,
    ) -> impl core::future::Future<Output = Result<(), Self::Error>> {
        self.0.erase(block_addr, block_addr + T::ERASE_SIZE as u32)
    }
}
