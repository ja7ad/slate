use crate::io_util::{read_exact_at, write_all_at};
use slate_kv_hal::Flash;
use std::fs::File;
#[cfg(target_os = "macos")]
use std::os::unix::io::AsRawFd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    Full,
    OsCache,
}

#[cfg(target_os = "macos")]
fn flush_durable(f: &File, durability: Durability) -> std::io::Result<()> {
    match durability {
        Durability::Full => {
            // SAFETY-equivalent of §2.3(2) ordering on Darwin: fsync alone may leave
            // writes in the drive cache; F_FULLFSYNC forces them to stable media.
            let rc = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) };
            if rc == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
        Durability::OsCache => f.sync_data(),
    }
}

#[cfg(not(target_os = "macos"))]
fn flush_durable(f: &File, _durability: Durability) -> std::io::Result<()> {
    f.sync_data()
}

#[derive(Debug)]
pub enum FileFlashError {
    Io(std::io::Error),
    Unaligned,
    OutOfBounds,
    ProgramWithoutErase,
}

impl From<std::io::Error> for FileFlashError {
    fn from(err: std::io::Error) -> Self {
        FileFlashError::Io(err)
    }
}

pub struct FileFlash {
    file: File,
    capacity: u32,
    page_size: usize,
    block_size: usize,
    durability: Durability,
}

impl FileFlash {
    pub fn new(
        file: File,
        capacity: u32,
        page_size: usize,
        block_size: usize,
        durability: Durability,
    ) -> Result<Self, std::io::Error> {
        let meta = file.metadata()?;
        if meta.len() != capacity as u64 {
            file.set_len(capacity as u64)?;
            // Fill with 0xFF if new
            let ff = vec![0xFF; capacity as usize];
            write_all_at(&file, &ff, 0)?;
            flush_durable(&file, durability)?;
        }
        Ok(Self {
            file,
            capacity,
            page_size,
            block_size,
            durability,
        })
    }
}

impl Flash for FileFlash {
    type Error = FileFlashError;

    fn page_size(&self) -> usize {
        self.page_size
    }

    fn block_size(&self) -> usize {
        self.block_size
    }

    fn capacity(&self) -> u32 {
        self.capacity
    }

    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        let addr = addr as usize;
        if addr + buf.len() > self.capacity as usize {
            return Err(FileFlashError::OutOfBounds);
        }
        read_exact_at(&self.file, buf, addr as u64)?;
        Ok(())
    }

    fn program(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        let addr = addr as usize;
        if !addr.is_multiple_of(self.page_size) || !buf.len().is_multiple_of(self.page_size) {
            return Err(FileFlashError::Unaligned);
        }
        if addr + buf.len() > self.capacity as usize {
            return Err(FileFlashError::OutOfBounds);
        }

        // Verify erased
        let mut check_buf = vec![0u8; buf.len()];
        read_exact_at(&self.file, &mut check_buf, addr as u64)?;
        if !check_buf.iter().all(|&b| b == 0xFF) {
            return Err(FileFlashError::ProgramWithoutErase);
        }

        // Write and sync
        write_all_at(&self.file, buf, addr as u64)?;
        flush_durable(&self.file, self.durability)?;
        Ok(())
    }

    fn erase(&mut self, block_addr: u32) -> Result<(), Self::Error> {
        let block_addr = block_addr as usize;
        if !block_addr.is_multiple_of(self.block_size) {
            return Err(FileFlashError::Unaligned);
        }
        if block_addr + self.block_size > self.capacity as usize {
            return Err(FileFlashError::OutOfBounds);
        }

        let ff = vec![0xFF; self.block_size];
        write_all_at(&self.file, &ff, block_addr as u64)?;
        flush_durable(&self.file, self.durability)?;
        Ok(())
    }
}
