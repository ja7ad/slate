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

pub struct Stats {
    pub commits: u64,
    pub wakes: u64,
    pub user_bytes: u64,
    pub gc_bytes: u64,
    pub parity_bytes: u64,
    pub ckpt_bytes: u64,
    pub erases: u64,
}

pub fn report(stats: &Stats, m: &PowerModel) -> PowerReport {
    let bytes = stats.user_bytes + stats.gc_bytes + stats.parity_bytes + stats.ckpt_bytes;
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
