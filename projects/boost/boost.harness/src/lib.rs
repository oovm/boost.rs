#![doc = include_str!("../readme.md")]
#![deny(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[cfg(feature = "std")]
pub mod benchmark;
pub mod diagnostic;
#[cfg(feature = "std")]
pub mod log;
#[cfg(feature = "std")]
pub mod test;
