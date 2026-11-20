#![doc = include_str!("readme.md")]
#![warn(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[cfg(feature = "std")]
pub mod benchmark;
pub mod diagnostic;
#[cfg(feature = "std")]
pub mod hotspot;
#[cfg(feature = "std")]
pub mod log;
#[cfg(feature = "std")]
pub mod test;
