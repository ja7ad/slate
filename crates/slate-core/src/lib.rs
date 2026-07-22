//! slate-core

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod chain;
pub mod checkpoint;
pub mod epoch;
pub mod error;
pub mod index;
pub mod log;
pub mod record;
pub mod recover;
pub mod segment;
