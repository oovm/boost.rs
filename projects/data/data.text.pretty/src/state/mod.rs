use std::{collections::HashMap, marker::PhantomData};

use data_text_core::language::Language;
use serde_json::Value;

use crate::document::PrinterConfig;

/// Formatting state alias used by [`FormatRule`](crate::FormatRule) defaults.
pub type FormatState = DefaultFormatState;

/// Context passed through incremental-tree [`FormatRule`](crate::FormatRule) application.
#[derive(Debug, Clone)]
pub struct FormatContext<L: Language, C: Clone, S: Clone = FormatState> {
    /// Language-specific formatter configuration.
    pub config: C,
    /// Mutable formatting state (indent, overrides).
    pub state: S,
    /// Printer configuration for final document rendering.
    pub printer_config: PrinterConfig,
    _language: PhantomData<L>,
}

impl<L: Language, C: Clone, S: Default + Clone> FormatContext<L, C, S> {
    /// Creates a context with the given configuration, default state and printer config.
    pub fn new(config: C, printer_config: PrinterConfig) -> Self {
        Self { config, state: S::default(), printer_config, _language: PhantomData }
    }
}

/// Default formatting state implementation.
#[derive(Debug, Clone, Default)]
pub struct DefaultFormatState {
    /// Local configuration overrides.
    pub local_config: HashMap<String, Value>,
    /// Custom state values.
    pub custom_state: HashMap<String, Value>,
    /// Current indentation level.
    pub indent_level: usize,
    /// Whether to force single-line formatting.
    pub force_single_line: bool,
    /// Whether to align elements.
    pub align_elements: bool,
}
