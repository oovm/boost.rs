#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

mod engine;
mod terminal;

pub use engine::{EchoRestore, EngineDeps, IoEvent, PromptEngine, PromptIo};
pub use terminal::{TerminalFactory, TerminalPromptIo};
