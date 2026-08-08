//! SLATE backend for the RP2040 / RP2350 (Raspberry Pi Pico family).
//!
//! # Why this port is straightforward, and where the hazard is
//!
//! The Pico's flash is an EXTERNAL QSPI NOR part (a W25Q16 on the reference
//! board): 4 KiB sector erase, 256 B page program. That is exactly the geometry
//! `slate-kv-core`'s format was designed around, so no format change is needed
//! — unlike STM32 internal flash, which programs in 2-32 byte words and cannot
//! hold the 83-byte commit marker in a single `program` call.
//!
//! The hazard is that the SAME chip holds the executing program, reached
//! through the RP2040's XIP (execute-in-place) cache. A flash write must
//! therefore:
//!
//! 1. run entirely from RAM — if the CPU fetches an instruction from flash
//!    while the SSI is in command mode, it fetches garbage and the core faults;
//! 2. run with interrupts masked, for the same reason (an ISR living in flash
//!    is an instruction fetch);
//! 3. flush the XIP cache afterwards, or subsequent reads are served stale
//!    data from the cache rather than the bytes just programmed.
//!
//! `rp2040_hal::rom_data` provides the ROM routines for this
//! (`connect_internal_flash`, `flash_exit_xip`, `flash_range_erase`,
//! `flash_range_program`, `flash_flush_cache`, `flash_enter_cmd_xip`), and
//! `#[link_section = ".data.ram_func"]` places the wrapper in RAM.
//!
//! # Region placement
//!
//! `SLATE_FLASH_BASE` must clear the program image. The linker knows where the
//! image ends, but a `no_std` binary cannot easily read that at runtime, so the
//! base is a build-time constant that the caller is responsible for setting
//! above its own `.text`/`.rodata`. [`slate_region_ok`] checks the region can
//! hold the reserved layout, and the demo prints the check at boot rather than
//! letting a too-small region surface as an opaque `Io` on first commit.

#![no_std]

use core::mem::MaybeUninit;
use slate_kv_hal::{CounterKind, Flash, MonotonicCounter};

/// Erase-block size of the QSPI NOR part on the Pico family.
pub const FLASH_BLOCK_SIZE: usize = 4096;

/// Program-page size. Must be >= `slate_kv_core::config::CM_LEN` (83): the
/// commit marker is written by ONE `program` call, and a shorter page would
/// truncate it (docs/design/019 §3.1).
pub const FLASH_PAGE_SIZE: usize = 256;

const _PAGE_HOLDS_MARKER: () = assert!(
    FLASH_PAGE_SIZE >= slate_kv_core::config::CM_LEN,
    "FLASH_PAGE_SIZE must be at least CM_LEN or commit markers are truncated"
);

/// Base of the SLATE region as an offset from the start of flash.
///
/// 1 MiB clears the program image on every demo build here with room to spare.
/// The ROM flash routines take offsets from the start of flash, NOT XIP
/// addresses, which is the opposite convention from reads (see [`XIP_BASE`]).
pub const SLATE_FLASH_BASE: u32 = 0x100_000;

/// Length of the SLATE region: 1 MiB, leaving the top of a 2 MiB part free.
pub const SLATE_FLASH_LEN: u32 = 0x100_000;

/// Memory-mapped base of the XIP window. Reads go through here; erase/program
/// take bare offsets. Mixing the two conventions is the classic RP2040 flash
/// bug — it silently targets an address 0x10000000 away from the intended one.
pub const XIP_BASE: u32 = 0x1000_0000;

/// Errors from [`Rp2Flash`].
#[derive(Debug)]
pub enum Rp2FlashError {
    /// `addr` or length was not page-aligned on program, or block-aligned on
    /// erase.
    Unaligned,
    /// The access fell outside `[base, base + len)`.
    OutOfBounds,
    /// A page was programmed while not in the erased state. The NOR part would
    /// accept the write (bits only ever go 1 -> 0), which is exactly why this
    /// has to be caught here: the format's program-once-per-erase contract is
    /// what makes a torn write detectable.
    ProgramWithoutErase,
}

/// SLATE [`Flash`] over the RP2040/RP2350 external QSPI NOR.
pub struct Rp2Flash {
    base: u32,
    len: u32,
}

impl Rp2Flash {
    /// Maps a SLATE region at `[base, base + len)` as offsets from the start of
    /// flash.
    ///
    /// # Panics
    ///
    /// If the region cannot hold the reserved checkpoint layout plus one
    /// segment. That configuration is undiagnosable later: mount succeeds, the
    /// write head sits above `capacity()`, and every commit fails with a bare
    /// `Io`.
    pub fn new(base: u32, len: u32) -> Self {
        assert!(
            base.is_multiple_of(FLASH_BLOCK_SIZE as u32),
            "SLATE base must be erase-block aligned"
        );
        assert!(
            slate_region_ok(len, 1),
            "SLATE region too small for the reserved layout plus one segment"
        );
        Self { base, len }
    }
}

/// Returns `true` if `len` bytes can hold the reserved checkpoint layout plus
/// at least `min_segments` GC segments.
pub fn slate_region_ok(len: u32, min_segments: u32) -> bool {
    let data_base = slate_kv_core::config::data_base_offset(FLASH_BLOCK_SIZE);
    let need = data_base as u64 + min_segments as u64 * slate_kv_core::config::SEG_BYTES as u64;
    (len as u64) >= need
}

/// Number of whole GC segments that fit above the reserved region.
pub fn slate_segment_capacity(len: u32) -> u32 {
    let data_base = slate_kv_core::config::data_base_offset(FLASH_BLOCK_SIZE);
    if len <= data_base {
        return 0;
    }
    (len - data_base) / slate_kv_core::config::SEG_BYTES as u32
}

/// Erases `count` bytes at `offset`, with XIP disabled and interrupts masked.
///
/// # Safety
///
/// `offset` and `count` must be 4 KiB-aligned and inside the flash part. The
/// function must not be inlined into a flash-resident caller: it is placed in
/// RAM because the CPU cannot fetch instructions from flash while the SSI is in
/// command mode.
#[link_section = ".data.ram_func"]
#[inline(never)]
unsafe fn ram_erase(offset: u32, count: usize) {
    cortex_m::interrupt::free(|_| {
        rp2040_hal::rom_data::connect_internal_flash();
        rp2040_hal::rom_data::flash_exit_xip();
        // 0xD8 is the W25Q block-erase command the ROM uses for 64 KiB blocks;
        // passing a 4096 block_size with cmd 0x20 (sector erase) keeps the
        // granularity at one sector, which is what the format's erase unit is.
        rp2040_hal::rom_data::flash_range_erase(offset, count, FLASH_BLOCK_SIZE as u32, 0x20);
        rp2040_hal::rom_data::flash_flush_cache();
        rp2040_hal::rom_data::flash_enter_cmd_xip();
    });
}

/// Programs `data` at `offset`. Same RAM/interrupt constraints as [`ram_erase`].
///
/// # Safety
///
/// `offset` and `data.len()` must be 256 B-aligned and inside the flash part.
#[link_section = ".data.ram_func"]
#[inline(never)]
unsafe fn ram_program(offset: u32, data: &[u8]) {
    cortex_m::interrupt::free(|_| {
        rp2040_hal::rom_data::connect_internal_flash();
        rp2040_hal::rom_data::flash_exit_xip();
        rp2040_hal::rom_data::flash_range_program(offset, data.as_ptr(), data.len());
        rp2040_hal::rom_data::flash_flush_cache();
        rp2040_hal::rom_data::flash_enter_cmd_xip();
    });
}

impl Flash for Rp2Flash {
    type Error = Rp2FlashError;

    fn page_size(&self) -> usize {
        FLASH_PAGE_SIZE
    }
    fn block_size(&self) -> usize {
        FLASH_BLOCK_SIZE
    }
    fn capacity(&self) -> u32 {
        self.len
    }

    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        if addr as u64 + buf.len() as u64 > self.len as u64 {
            return Err(Rp2FlashError::OutOfBounds);
        }
        // Reads are plain memory loads through the XIP window -- no ROM call,
        // no XIP disable, and unaligned lengths are fine. This is why the port
        // needs no equivalent of the ESP32 backend's alignment-fixing read
        // path: SLATE reads 28-byte headers at arbitrary offsets, and the XIP
        // window serves them directly.
        let src = (XIP_BASE + self.base + addr) as *const u8;
        // SAFETY: the range was bounds-checked against the mapped region above,
        // and the XIP window is readable for the whole flash part.
        unsafe { core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), buf.len()) };
        Ok(())
    }

    fn program(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        if !addr.is_multiple_of(FLASH_PAGE_SIZE as u32)
            || !buf.len().is_multiple_of(FLASH_PAGE_SIZE)
        {
            return Err(Rp2FlashError::Unaligned);
        }
        if addr as u64 + buf.len() as u64 > self.len as u64 {
            return Err(Rp2FlashError::OutOfBounds);
        }

        // Erase verification, in release as well as debug. NOR programming can
        // only clear bits, so writing a non-erased page yields the AND of old
        // and new -- a silently corrupt record rather than a failure. The
        // format's tear detection assumes program-once-per-erase; this is what
        // enforces it.
        let mut chunk = [0u8; FLASH_PAGE_SIZE];
        for off in (0..buf.len()).step_by(FLASH_PAGE_SIZE) {
            self.read(addr + off as u32, &mut chunk)?;
            if chunk
                .iter()
                .any(|&b| b != slate_kv_core::config::ERASED_BYTE)
            {
                return Err(Rp2FlashError::ProgramWithoutErase);
            }
        }

        // SAFETY: alignment and bounds checked above; `ram_program` runs from
        // RAM with interrupts masked.
        unsafe { ram_program(self.base + addr, buf) };
        Ok(())
    }

    fn erase(&mut self, block_addr: u32) -> Result<(), Self::Error> {
        if !block_addr.is_multiple_of(FLASH_BLOCK_SIZE as u32) {
            return Err(Rp2FlashError::Unaligned);
        }
        if block_addr as u64 + FLASH_BLOCK_SIZE as u64 > self.len as u64 {
            return Err(Rp2FlashError::OutOfBounds);
        }
        // SAFETY: as above.
        unsafe { ram_erase(self.base + block_addr, FLASH_BLOCK_SIZE) };
        Ok(())
    }
}

/// Errors from [`Rp2Counter`].
#[derive(Debug)]
pub enum Rp2CounterError {
    /// The 64-bit counter space is exhausted.
    Exhausted,
}

/// Flash offset where the best-effort rollback counter persists, in its own
/// erase sector OUTSIDE every SLATE data region so the two never share a
/// sector.
pub const COUNTER_FLASH_ADDR: u32 = SLATE_FLASH_BASE + SLATE_FLASH_LEN;

/// Best-effort monotonic counter for the RP2 family.
///
/// The RP2040 has NO hardware monotonic counter and no eFuse counter usable for
/// rollback protection, so [`CounterKind::BestEffort`] is the honest answer:
/// the value survives reboot but an attacker with physical flash access can
/// roll it back. `kind()` reports this truthfully so the engine can degrade its
/// freshness claim rather than overstate it (report §2.4 / §3.4).
pub struct Rp2Counter {
    val: u64,
    persist: bool,
}

impl Default for Rp2Counter {
    fn default() -> Self {
        Self::new()
    }
}

impl Rp2Counter {
    /// Loads the persisted value, or starts at 0 on an erased sector.
    pub fn new() -> Self {
        let mut buf = [0u8; 8];
        let src = (XIP_BASE + COUNTER_FLASH_ADDR) as *const u8;
        // SAFETY: reading 8 bytes through the XIP window inside the part.
        unsafe { core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), 8) };
        let stored = u64::from_le_bytes(buf);
        Self {
            // An erased sector reads as all-ones; treat that as "never written".
            val: if stored == u64::MAX { 0 } else { stored },
            persist: true,
        }
    }

    /// A RAM-only counter, for tests and for boards with no spare sector.
    pub fn volatile() -> Self {
        Self {
            val: 0,
            persist: false,
        }
    }
}

impl MonotonicCounter for Rp2Counter {
    type Error = Rp2CounterError;

    fn kind(&self) -> CounterKind {
        if self.persist {
            CounterKind::BestEffort
        } else {
            CounterKind::None
        }
    }

    fn read(&mut self) -> Result<u64, Self::Error> {
        Ok(self.val)
    }

    fn increment(&mut self) -> Result<u64, Self::Error> {
        self.val = self.val.checked_add(1).ok_or(Rp2CounterError::Exhausted)?;
        if self.persist {
            // Erase-then-write the whole sector. This is why the counter needs
            // its own sector: sharing one with log data would erase records.
            let mut page = [0xFFu8; FLASH_PAGE_SIZE];
            page[..8].copy_from_slice(&self.val.to_le_bytes());
            // SAFETY: COUNTER_FLASH_ADDR is sector-aligned by construction and
            // disjoint from every SLATE data region.
            unsafe {
                ram_erase(COUNTER_FLASH_ADDR, FLASH_BLOCK_SIZE);
                ram_program(COUNTER_FLASH_ADDR, &page);
            }
        }
        Ok(self.val)
    }
}

/// Zero-cost thread-safe wrapper around a static buffer, mirroring the ESP32
/// port's `SyncBuffer` so the demo binaries look the same on both targets.
pub struct SyncBuffer<T> {
    name: &'static str,
    data: core::cell::UnsafeCell<MaybeUninit<T>>,
    taken: core::cell::Cell<bool>,
}

// SAFETY: `take` is serialised by a critical section and panics on a second
// call, so at most one `&'static mut T` ever exists.
unsafe impl<T> Sync for SyncBuffer<T> {}

impl<T> SyncBuffer<T> {
    /// Creates a buffer holding `value`.
    pub const fn new(name: &'static str, value: T) -> Self {
        Self {
            name,
            data: core::cell::UnsafeCell::new(MaybeUninit::new(value)),
            taken: core::cell::Cell::new(false),
        }
    }

    /// Takes the buffer. Panics if called twice.
    #[allow(clippy::mut_from_ref)]
    pub fn take(&self) -> &'static mut T {
        critical_section::with(|_cs| {
            if self.taken.get() {
                panic!("SyncBuffer {} already taken", self.name);
            }
            self.taken.set(true);
            // SAFETY: initialised in `new`, and this is the only handout.
            unsafe { (*self.data.get()).assume_init_mut() }
        })
    }
}
