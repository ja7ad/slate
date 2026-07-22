//! slate-sim
#![allow(clippy::manual_is_multiple_of)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::collapsible_if)]

use slate_hal::{CounterKind, Flash, MonotonicCounter};
use std::collections::BTreeSet;

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub programs: u64,
    pub erases: u64,
    pub bytes_programmed: u64,
    pub wakes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crash {
    None,
    AtByte { op_index: u64, byte_in_op: usize },
}

#[derive(Debug, Clone)]
pub struct PowerModel {
    pub current_op: u64,
    pub crash: Crash,
}

#[derive(Debug)]
pub enum SimFlashError {
    PowerLoss,
    Unaligned,
    OutOfBounds,
    AlreadyProgrammed,
    BadBlock,
}

pub struct SimFlash {
    pub mem: Vec<u8>,
    pub programmed: Vec<bool>,
    pub bad_blocks: BTreeSet<u32>,
    pub power: PowerModel,
    pub stats: Stats,
    page_size: usize,
    block_size: usize,
}

impl SimFlash {
    pub fn new(capacity: u32, page_size: usize, block_size: usize) -> Self {
        let pages = (capacity as usize) / page_size;
        Self {
            mem: vec![0xFF; capacity as usize],
            programmed: vec![false; pages],
            bad_blocks: BTreeSet::new(),
            power: PowerModel {
                current_op: 0,
                crash: Crash::None,
            },
            stats: Stats::default(),
            page_size,
            block_size,
        }
    }
}

impl Flash for SimFlash {
    type Error = SimFlashError;

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
            return Err(SimFlashError::OutOfBounds);
        }
        let block = (addr / self.block_size) as u32;
        if self.bad_blocks.contains(&block) {
            return Err(SimFlashError::BadBlock);
        }
        buf.copy_from_slice(&self.mem[addr..addr + buf.len()]);
        Ok(())
    }

    fn program(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        let addr = addr as usize;
        if addr % self.page_size != 0 || buf.len() % self.page_size != 0 {
            return Err(SimFlashError::Unaligned);
        }
        if addr + buf.len() > self.mem.len() {
            return Err(SimFlashError::OutOfBounds);
        }

        let op_idx = self.power.current_op;
        self.power.current_op += 1;

        let crash_byte = match self.power.crash {
            Crash::AtByte {
                op_index,
                byte_in_op,
            } if op_index == op_idx => Some(byte_in_op),
            _ => None,
        };

        let num_pages = buf.len() / self.page_size;
        let start_page = addr / self.page_size;
        for i in 0..num_pages {
            if self.programmed[start_page + i] {
                return Err(SimFlashError::AlreadyProgrammed);
            }
        }

        let bytes_to_write = crash_byte.unwrap_or(buf.len());
        // Since it's NOR flash, we can only flip 1s to 0s
        for i in 0..bytes_to_write {
            self.mem[addr + i] &= buf[i];
        }

        self.stats.programs += 1;
        self.stats.bytes_programmed += bytes_to_write as u64;

        if crash_byte.is_some() {
            return Err(SimFlashError::PowerLoss);
        }

        for i in 0..num_pages {
            self.programmed[start_page + i] = true;
        }

        Ok(())
    }

    fn erase(&mut self, block_addr: u32) -> Result<(), Self::Error> {
        let block_addr = block_addr as usize;
        if block_addr % self.block_size != 0 {
            return Err(SimFlashError::Unaligned);
        }
        if block_addr + self.block_size > self.mem.len() {
            return Err(SimFlashError::OutOfBounds);
        }

        let op_idx = self.power.current_op;
        self.power.current_op += 1;

        let crash_byte = match self.power.crash {
            Crash::AtByte {
                op_index,
                byte_in_op,
            } if op_index == op_idx => Some(byte_in_op),
            _ => None,
        };

        let bytes_to_erase = crash_byte.unwrap_or(self.block_size);
        for i in 0..bytes_to_erase {
            self.mem[block_addr + i] = 0xFF;
        }

        self.stats.erases += 1;

        if crash_byte.is_some() {
            return Err(SimFlashError::PowerLoss);
        }

        let start_page = block_addr / self.page_size;
        let num_pages = self.block_size / self.page_size;
        for i in 0..num_pages {
            self.programmed[start_page + i] = false;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub enum SimCounterError {
    PowerLoss,
    Exhausted,
}

pub struct SimCounter {
    pub val: u64,
    pub budget: u64,
    pub power: PowerModel,
}

impl SimCounter {
    pub fn new(budget: u64) -> Self {
        Self {
            val: 0,
            budget,
            power: PowerModel {
                current_op: 0,
                crash: Crash::None,
            },
        }
    }
}

impl MonotonicCounter for SimCounter {
    type Error = SimCounterError;

    fn kind(&self) -> CounterKind {
        CounterKind::Hardware
    }

    fn read(&mut self) -> Result<u64, Self::Error> {
        Ok(self.val)
    }

    fn increment(&mut self) -> Result<u64, Self::Error> {
        if self.budget == 0 {
            return Err(SimCounterError::Exhausted);
        }

        let op_idx = self.power.current_op;
        self.power.current_op += 1;

        if let Crash::AtByte { op_index, .. } = self.power.crash {
            if op_index == op_idx {
                return Err(SimCounterError::PowerLoss);
            }
        }

        self.budget -= 1;
        self.val += 1;
        Ok(self.val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn test_sim_flash_crash(
            op_idx in 0u64..10,
            byte_idx in 0usize..256
        ) {
            let mut flash = SimFlash::new(4096, 256, 4096);
            flash.power.crash = Crash::AtByte { op_index: op_idx, byte_in_op: byte_idx };

            let mut success = true;
            for i in 0..10 {
                let buf = std::vec![i as u8; 256];
                match flash.program((i * 256) as u32, &buf) {
                    Ok(_) => {}
                    Err(SimFlashError::PowerLoss) => {
                        success = false;

                        // Check torn page
                        let mut read_buf = std::vec![0u8; 256];
                        flash.read((i * 256) as u32, &mut read_buf).unwrap();
                        for j in 0..byte_idx {
                            assert_eq!(read_buf[j], i as u8, "failed at index {}", j);
                        }
                        for j in byte_idx..256 {
                            assert_eq!(read_buf[j], 0xFF, "failed at index {}", j);
                        }
                        break;
                    }
                    _ => panic!("unexpected error"),
                }
            }
            if !success {
                assert!(flash.power.current_op > 0);
            }
        }
    }

    #[test]
    fn test_sim_counter() {
        let mut counter = SimCounter::new(2);
        assert_eq!(counter.increment().unwrap(), 1);
        assert_eq!(counter.increment().unwrap(), 2);
        assert!(matches!(
            counter.increment(),
            Err(SimCounterError::Exhausted)
        ));
    }
}
