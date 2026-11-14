//! Minimal runtime utilities for DejaVu AOT and host helpers.

mod display;
pub mod escaper;
pub mod looper;
mod traits;

pub use self::{
    display::{EscapeDisplay, Escaper, HtmlEscapeConfig, escape, escape_with_config, safe_html_attribute_escape, safe_html_escape},
    traits::Template,
};
