//! Human-visible rendering and stderr subscriber for structured log events.

mod render;
mod sink;
mod subscriber;

pub use render::{render, render_plain};
pub use sink::{StderrFallbackSink, StderrSubscriberSink};
pub use subscriber::install_global_subscriber;
