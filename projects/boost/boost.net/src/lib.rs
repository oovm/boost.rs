#![doc = include_str!("readme.md")]
#![deny(missing_docs)]

pub mod http;
pub mod tls;
pub mod ws;

mod connection;
mod error;

pub use error::NetError;
pub use url::Url;
