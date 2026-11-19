#![doc = include_str!("readme.md")]
#![warn(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod http;
pub mod tls;
#[cfg(feature = "std")]
pub mod ws;

#[cfg(feature = "std")]
mod connection;
mod error;

pub use error::NetError;
pub use url::Url;
