use slate_hal::{CounterKind, MonotonicCounter};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use hmac::{Hmac, Mac, KeyInit};
use sha2::Sha256;

#[cfg(target_os = "macos")]
fn flush_durable(f: &File) -> std::io::Result<()> {
    let rc = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) };
    if rc == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
fn flush_durable(f: &File) -> std::io::Result<()> {
    f.sync_data()
}

type HmacSha256 = Hmac<Sha256>;

const SLOT_SIZE: usize = 40; // 8 bytes value + 32 bytes HMAC

#[derive(Debug)]
pub enum FileCounterError {
    Io(std::io::Error),
    Exhausted,
    FormatError,
}

impl From<std::io::Error> for FileCounterError {
    fn from(err: std::io::Error) -> Self {
        FileCounterError::Io(err)
    }
}

pub struct FileCounter {
    file: File,
    key: [u8; 32],
    budget: u64,
    val: u64,
}

impl FileCounter {
    pub fn new(file: File, key: [u8; 32], budget: u64) -> Result<Self, FileCounterError> {
        let meta = file.metadata()?;
        if meta.len() != 80 {
            file.set_len(80)?;
            // Init both slots to 0
            let mut c = Self { file, key, budget, val: 0 };
            c.write_slot(0, 0)?;
            c.write_slot(1, 0)?;
            return Ok(c);
        }

        let mut c = Self { file, key, budget, val: 0 };
        c.val = c.read_max()?;
        Ok(c)
    }

    fn read_max(&self) -> Result<u64, FileCounterError> {
        let v0 = self.read_slot(0);
        let v1 = self.read_slot(1);
        match (v0, v1) {
            (Ok(a), Ok(b)) => Ok(a.max(b)),
            (Ok(a), Err(_)) => Ok(a),
            (Err(_), Ok(b)) => Ok(b),
            (Err(e), Err(_)) => Err(e),
        }
    }

    fn read_slot(&self, idx: u64) -> Result<u64, FileCounterError> {
        let mut buf = [0u8; SLOT_SIZE];
        self.file.read_exact_at(&mut buf, idx * SLOT_SIZE as u64)?;
        
        let mut val_bytes = [0u8; 8];
        val_bytes.copy_from_slice(&buf[0..8]);
        let val = u64::from_le_bytes(val_bytes);
        
        let mut mac = HmacSha256::new_from_slice(&self.key).unwrap();
        mac.update(&val_bytes);
        
        if mac.verify_slice(&buf[8..40]).is_ok() {
            Ok(val)
        } else {
            Err(FileCounterError::FormatError)
        }
    }

    fn write_slot(&mut self, idx: u64, val: u64) -> Result<(), FileCounterError> {
        let mut buf = [0u8; SLOT_SIZE];
        let val_bytes = val.to_le_bytes();
        buf[0..8].copy_from_slice(&val_bytes);
        
        let mut mac = HmacSha256::new_from_slice(&self.key).unwrap();
        mac.update(&val_bytes);
        let tag = mac.finalize().into_bytes();
        buf[8..40].copy_from_slice(&tag);
        
        self.file.write_all_at(&buf, idx * SLOT_SIZE as u64)?;
        flush_durable(&self.file)?;
        Ok(())
    }
}

impl MonotonicCounter for FileCounter {
    type Error = FileCounterError;

    fn kind(&self) -> CounterKind {
        CounterKind::BestEffort
    }

    fn read(&mut self) -> Result<u64, Self::Error> {
        Ok(self.val)
    }

    fn increment(&mut self) -> Result<u64, Self::Error> {
        if self.budget == 0 {
            return Err(FileCounterError::Exhausted);
        }
        
        // Find which slot to overwrite: the older one
        let v0 = self.read_slot(0).unwrap_or(0);
        let v1 = self.read_slot(1).unwrap_or(0);
        
        let target_idx = if v0 <= v1 { 0 } else { 1 };
        
        self.val += 1;
        self.budget -= 1;
        
        self.write_slot(target_idx, self.val)?;
        Ok(self.val)
    }
}
