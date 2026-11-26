//! Static tables defined by RFC 7932.
//!
//! The values are transcribed from the `brotli-decompressor` 5.0.3 sources
//! (MIT licensed), which ship the reference tables; this crate does not depend
//! on that crate at runtime.

mod command;
mod constants;

pub(crate) use command::cmd_lut;
pub(crate) use constants::{BLOCK_LEN_PREFIX, CONTEXT_LOOKUP, PREFIX_SUFFIX, TRANSFORMS};
