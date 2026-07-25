use std::fs::File;
use std::io::Result;

#[cfg(unix)]
use std::os::unix::fs::FileExt;

#[cfg(windows)]
use std::os::windows::fs::FileExt;

#[allow(unused_mut)]
pub fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> Result<()> {
    #[cfg(unix)]
    {
        file.read_exact_at(buf, offset)
    }

    #[cfg(windows)]
    {
        while !buf.is_empty() {
            let n = file.seek_read(buf, offset)?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "failed to fill whole buffer",
                ));
            }
            let slice = buf;
            buf = &mut slice[n..];
            offset += n as u64;
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, buf, offset);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "positional I/O unsupported on this platform",
        ))
    }
}

#[allow(unused_mut)]
pub fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> Result<()> {
    #[cfg(unix)]
    {
        file.write_all_at(buf, offset)
    }

    #[cfg(windows)]
    {
        while !buf.is_empty() {
            let n = file.seek_write(buf, offset)?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            buf = &buf[n..];
            offset += n as u64;
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, buf, offset);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "positional I/O unsupported on this platform",
        ))
    }
}
