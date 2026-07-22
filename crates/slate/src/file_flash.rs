use slate_hal::Flash;
use std::fs::File;
use std::os::unix::fs::FileExt;

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
}

impl FileFlash {
    pub fn new(file: File, capacity: u32, page_size: usize, block_size: usize) -> Result<Self, std::io::Error> {
        let meta = file.metadata()?;
        if meta.len() != capacity as u64 {
            file.set_len(capacity as u64)?;
            // Fill with 0xFF if new
            let ff = vec![0xFF; capacity as usize];
            file.write_all_at(&ff, 0)?;
            file.sync_data()?;
        }
        Ok(Self {
            file,
            capacity,
            page_size,
            block_size,
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
        self.file.read_exact_at(buf, addr as u64)?;
        Ok(())
    }

    fn program(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        let addr = addr as usize;
        if addr % self.page_size != 0 || buf.len() % self.page_size != 0 {
            return Err(FileFlashError::Unaligned);
        }
        if addr + buf.len() > self.capacity as usize {
            return Err(FileFlashError::OutOfBounds);
        }

        // Verify erased
        let mut check_buf = vec![0u8; buf.len()];
        self.file.read_exact_at(&mut check_buf, addr as u64)?;
        if !check_buf.iter().all(|&b| b == 0xFF) {
            return Err(FileFlashError::ProgramWithoutErase);
        }

        // Write and sync
        self.file.write_all_at(buf, addr as u64)?;
        self.file.sync_data()?;
        Ok(())
    }

    fn erase(&mut self, block_addr: u32) -> Result<(), Self::Error> {
        let block_addr = block_addr as usize;
        if block_addr % self.block_size != 0 {
            return Err(FileFlashError::Unaligned);
        }
        if block_addr + self.block_size > self.capacity as usize {
            return Err(FileFlashError::OutOfBounds);
        }

        let ff = vec![0xFF; self.block_size];
        self.file.write_all_at(&ff, block_addr as u64)?;
        self.file.sync_data()?;
        Ok(())
    }
}
