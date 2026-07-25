//! slate-kv-hal

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_is_multiple_of)]
#![allow(clippy::needless_range_loop)]

/// Report §2.1: program-once-per-erase NOR flash with detectable tearing.
pub trait Flash {
    /// Flash error type.
    type Error: core::fmt::Debug;
    /// Program granularity.
    fn page_size(&self) -> usize;
    /// Erase granularity, multiple of page_size.
    fn block_size(&self) -> usize;
    /// Capacity in bytes, C_flash.
    fn capacity(&self) -> u32;
    /// Reads from flash.
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error>;
    /// May only program pages that are in erased state. addr and buf.len()
    /// must be page-aligned/page-sized multiples. Completion = durable.
    fn program(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error>;
    /// Erases a block resetting it to all 1s.
    fn erase(&mut self, block_addr: u32) -> Result<(), Self::Error>;
}

/// Type of monotonic counter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CounterKind {
    /// Hardware counter.
    Hardware,
    /// Best effort counter.
    BestEffort,
    /// No counter.
    None,
}

/// Report §2.4 / §3.4: hardware monotonic counter, honest-degradation aware.
pub trait MonotonicCounter {
    /// Counter error type.
    type Error: core::fmt::Debug;
    /// Kind of the counter.
    fn kind(&self) -> CounterKind;
    /// Reads the counter.
    fn read(&mut self) -> Result<u64, Self::Error>;
    /// Must be durable before returning Ok. Fails when budget exhausted.
    fn increment(&mut self) -> Result<u64, Self::Error>; // returns new value
}

/// Source of entropy.
pub trait EntropySource {
    /// Fills buffer with entropy.
    fn fill(&mut self, buf: &mut [u8]);
}

/// Optional coarse clock for the deadline clamp (doc 008); ticks in ms.
pub trait Clock {
    /// Returns current time in ms.
    fn now_ms(&self) -> u64;
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[derive(Debug)]
    pub struct StubFlashError;

    pub struct StubFlash {
        pub mem: Vec<u8>,
        page_size: usize,
        block_size: usize,
    }

    impl StubFlash {
        pub fn new(capacity: u32, page_size: usize, block_size: usize) -> Self {
            Self {
                mem: std::vec![0xFF; capacity as usize],
                page_size,
                block_size,
            }
        }
    }

    impl Flash for StubFlash {
        type Error = StubFlashError;

        fn page_size(&self) -> usize {
            self.page_size
        }

        fn block_size(&self) -> usize {
            self.block_size
        }

        fn capacity(&self) -> u32 {
            self.mem.len() as u32
        }

        fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
            let addr = addr as usize;
            if addr + buf.len() > self.mem.len() {
                return Err(StubFlashError);
            }
            buf.copy_from_slice(&self.mem[addr..addr + buf.len()]);
            Ok(())
        }

        fn program(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
            let addr = addr as usize;
            if addr % self.page_size != 0 || buf.len() % self.page_size != 0 {
                return Err(StubFlashError);
            }
            if addr + buf.len() > self.mem.len() {
                return Err(StubFlashError);
            }
            for i in 0..buf.len() {
                if self.mem[addr + i] != 0xFF {
                    return Err(StubFlashError); // Program-once-per-erase
                }
                self.mem[addr + i] &= buf[i];
            }
            Ok(())
        }

        fn erase(&mut self, block_addr: u32) -> Result<(), Self::Error> {
            let block_addr = block_addr as usize;
            if block_addr % self.block_size != 0 {
                return Err(StubFlashError);
            }
            if block_addr + self.block_size > self.mem.len() {
                return Err(StubFlashError);
            }
            for i in 0..self.block_size {
                self.mem[block_addr + i] = 0xFF;
            }
            Ok(())
        }
    }

    #[derive(Debug)]
    pub struct StubCounterError;

    pub struct StubCounter {
        val: u64,
        budget: u64,
        kind: CounterKind,
    }

    impl StubCounter {
        pub fn new(budget: u64, kind: CounterKind) -> Self {
            Self {
                val: 0,
                budget,
                kind,
            }
        }
    }

    impl MonotonicCounter for StubCounter {
        type Error = StubCounterError;

        fn kind(&self) -> CounterKind {
            self.kind
        }

        fn read(&mut self) -> Result<u64, Self::Error> {
            Ok(self.val)
        }

        fn increment(&mut self) -> Result<u64, Self::Error> {
            if self.budget == 0 {
                return Err(StubCounterError);
            }
            self.budget -= 1;
            self.val += 1;
            Ok(self.val)
        }
    }

    #[test]
    fn test_stub_flash() {
        let mut flash = StubFlash::new(4096, 256, 4096);
        assert_eq!(flash.capacity(), 4096);
        let mut buf = [0u8; 256];
        flash.read(0, &mut buf).unwrap();
        assert_eq!(buf, [0xFF; 256]);

        let prog_buf = [0xAA; 256];
        flash.program(0, &prog_buf).unwrap();
        flash.read(0, &mut buf).unwrap();
        assert_eq!(buf, [0xAA; 256]);

        assert!(flash.program(0, &prog_buf).is_err()); // Already programmed
        flash.erase(0).unwrap();
        flash.read(0, &mut buf).unwrap();
        assert_eq!(buf, [0xFF; 256]);
    }
}
