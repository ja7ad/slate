//! metrics
#![allow(missing_docs)]

#[cfg(feature = "metrics")]
#[derive(Default, Debug, Clone)]
pub struct Metrics {
    pub commits: u64,
    pub wakes: u64,
    /// Record bytes the application asked to store (framing + key + value).
    pub user_bytes: u64,
    /// Record bytes rewritten by GC relocation.
    pub gc_bytes: u64,
    /// XOR/RS parity pages programmed.
    pub parity_bytes: u64,
    /// Commit-marker pages programmed (two copies per commit).
    pub marker_bytes: u64,
    /// Checkpoint pages programmed by epoch seals.
    pub ckpt_bytes: u64,
    pub erases: u64,
    /// Records visited by a compaction scan.
    pub gc_scanned: u64,
    /// Records the scan found still live and relocated.
    pub gc_relocated: u64,
    /// Records the scan could not AEAD-open. A nonzero value here during
    /// reclaim means records were treated as garbage without being read, which
    /// is data loss rather than a statistic — it is surfaced deliberately.
    pub gc_open_failed: u64,
    /// Segments reclaimed.
    pub gc_segments_freed: u64,
}

#[cfg(feature = "metrics")]
impl Metrics {
    pub fn add_user_bytes(&mut self, b: u64) {
        self.user_bytes += b;
    }
    pub fn add_gc_bytes(&mut self, b: u64) {
        self.gc_bytes += b;
    }
    pub fn add_parity_bytes(&mut self, b: u64) {
        self.parity_bytes += b;
    }
    pub fn add_marker_bytes(&mut self, b: u64) {
        self.marker_bytes += b;
    }
    pub fn add_ckpt_bytes(&mut self, b: u64) {
        self.ckpt_bytes += b;
    }
    pub fn add_commit(&mut self) {
        self.commits += 1;
    }
    pub fn add_wake(&mut self) {
        self.wakes += 1;
    }
    pub fn add_erase(&mut self) {
        self.erases += 1;
    }
    pub fn add_gc_scanned(&mut self) {
        self.gc_scanned += 1;
    }
    pub fn add_gc_relocated(&mut self) {
        self.gc_relocated += 1;
    }
    pub fn add_gc_open_failed(&mut self) {
        self.gc_open_failed += 1;
    }
    pub fn add_gc_segment_freed(&mut self) {
        self.gc_segments_freed += 1;
    }

    /// Total bytes programmed to flash, across every bucket.
    pub fn flash_bytes(&self) -> u64 {
        self.user_bytes + self.gc_bytes + self.parity_bytes + self.marker_bytes + self.ckpt_bytes
    }

    /// Write amplification: bytes actually programmed per byte of user data.
    ///
    /// Returns `None` when no user bytes have been written, rather than a
    /// meaningless 1.0 — an unmeasured workload and a workload with no overhead
    /// are different claims and must not be reported identically.
    #[allow(clippy::float_arithmetic)]
    pub fn write_amplification(&self) -> Option<f32> {
        if self.user_bytes == 0 {
            None
        } else {
            Some(self.flash_bytes() as f32 / self.user_bytes as f32)
        }
    }
}

#[cfg(not(feature = "metrics"))]
#[derive(Default, Debug, Clone)]
pub struct Metrics {}

#[cfg(not(feature = "metrics"))]
impl Metrics {
    #[inline(always)]
    pub fn add_user_bytes(&mut self, _b: u64) {}
    #[inline(always)]
    pub fn add_gc_bytes(&mut self, _b: u64) {}
    #[inline(always)]
    pub fn add_parity_bytes(&mut self, _b: u64) {}
    #[inline(always)]
    pub fn add_marker_bytes(&mut self, _b: u64) {}
    #[inline(always)]
    pub fn add_ckpt_bytes(&mut self, _b: u64) {}
    #[inline(always)]
    pub fn flash_bytes(&self) -> u64 {
        0
    }
    #[inline(always)]
    #[allow(clippy::float_arithmetic)]
    pub fn write_amplification(&self) -> Option<f32> {
        None
    }
    #[inline(always)]
    pub fn add_commit(&mut self) {}
    #[inline(always)]
    pub fn add_wake(&mut self) {}
    #[inline(always)]
    pub fn add_erase(&mut self) {}
    #[inline(always)]
    pub fn add_gc_scanned(&mut self) {}
    #[inline(always)]
    pub fn add_gc_relocated(&mut self) {}
    #[inline(always)]
    pub fn add_gc_open_failed(&mut self) {}
    #[inline(always)]
    pub fn add_gc_segment_freed(&mut self) {}
}
