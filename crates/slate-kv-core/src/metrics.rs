//! metrics
#![allow(missing_docs)]

#[cfg(feature = "metrics")]
#[derive(Default, Debug, Clone)]
pub struct Metrics {
    pub commits: u64,
    pub wakes: u64,
    pub user_bytes: u64,
    pub gc_bytes: u64,
    pub parity_bytes: u64,
    pub ckpt_bytes: u64,
    pub erases: u64,
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
    pub fn add_ckpt_bytes(&mut self, _b: u64) {}
    #[inline(always)]
    pub fn add_commit(&mut self) {}
    #[inline(always)]
    pub fn add_wake(&mut self) {}
    #[inline(always)]
    pub fn add_erase(&mut self) {}
}
