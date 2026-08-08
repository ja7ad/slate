use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerModel {
    pub beta_nj_per_byte: u64,
    pub erase_uj_per_block: u64,
    pub wake_uj: u64,
    pub cpu_nj_per_cycle_q10: u64,
    pub aead_cycles_per_byte: u64,
}

impl Default for PowerModel {
    fn default() -> Self {
        Self {
            beta_nj_per_byte: 200,
            erase_uj_per_block: 5000,
            wake_uj: 1000,
            cpu_nj_per_cycle_q10: 1024,
            aead_cycles_per_byte: 24,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PowerReport {
    pub m_joules: f64,
    pub label: &'static str,
}

#[derive(Default, Clone, Debug)]
pub struct Stats {
    pub commits: u64,
    pub wakes: u64,
    pub user_bytes: u64,
    pub gc_bytes: u64,
    pub parity_bytes: u64,
    /// Commit-marker pages (two per commit).
    ///
    /// This bucket was missing entirely, and with it the energy model's
    /// largest overhead term: at `b_commit = 8` with 74-byte records, markers
    /// cost 64 B per record against 32 B of parity and 74 B of payload. Every
    /// energy figure computed without it understated write energy.
    pub marker_bytes: u64,
    pub ckpt_bytes: u64,
    pub erases: u64,
    /// Segments in the table.
    pub segments: u32,
    /// Records compaction could not decrypt. Nonzero means data loss.
    pub gc_open_failed: u64,
}

impl Stats {
    /// Bytes actually programmed, across every bucket.
    pub fn flash_bytes(&self) -> u64 {
        self.user_bytes + self.gc_bytes + self.parity_bytes + self.marker_bytes + self.ckpt_bytes
    }

    /// Bytes programmed per byte of user data, or `None` when nothing has been
    /// written — an unmeasured workload and one with no overhead are different
    /// claims and must not report identically.
    #[allow(clippy::float_arithmetic)]
    pub fn write_amplification(&self) -> Option<f64> {
        if self.user_bytes == 0 {
            return None;
        }
        Some(self.flash_bytes() as f64 / self.user_bytes as f64)
    }
}

pub fn report(stats: &Stats, m: &PowerModel) -> PowerReport {
    // Commit markers are two full pages per commit and dominate the overhead
    // at small batch sizes; omitting them here understated write energy.
    let bytes = stats.flash_bytes();
    let write_nj = bytes * m.beta_nj_per_byte;
    let erase_nj = stats.erases * m.erase_uj_per_block * 1000;
    let wake_nj = stats.wakes * m.wake_uj * 1000;
    let cpu_cycles = bytes * m.aead_cycles_per_byte;
    let cpu_nj = (cpu_cycles * m.cpu_nj_per_cycle_q10) / 1024;

    let total_nj = write_nj + erase_nj + wake_nj + cpu_nj;

    PowerReport {
        m_joules: total_nj as f64 / 1_000_000.0,
        label: "ESTIMATED",
    }
}
