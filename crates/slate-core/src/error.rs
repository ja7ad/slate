//! error
#![allow(missing_docs)]

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Tampered,
    Rollback,
    TornTail,
    BatchFull,
    FlashFull,
    WearOut,
    CounterExhausted,
    Io,
    FormatError,
    IndexFull,
}
