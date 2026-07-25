//! slate-kv-core

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![deny(clippy::float_arithmetic)]

pub mod chain;
pub mod checkpoint;
pub mod config;
pub mod epoch;
pub mod error;
pub mod gc;
pub mod index;
pub mod log;
pub mod metrics;
pub mod record;
pub mod recover;
pub mod repair;
pub mod sched;
pub mod segment;
pub mod slate;
