#![doc = include_str!("../readme.md")]
#![deny(missing_docs)]

mod engine;
mod terminal;

pub use engine::{EchoRestore, EngineDeps, IoEvent, PromptEngine, PromptIo};
pub use terminal::{TerminalFactory, TerminalPromptIo};
