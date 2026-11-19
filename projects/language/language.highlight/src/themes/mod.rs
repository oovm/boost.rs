//! Predefined themes and theme selection.
//!
//! Absorbed from the former `oak-highlight` crate (MPL-2.0).

/// One Dark Pro theme.
pub mod one_dark_pro;
/// One Light theme.
pub mod one_light;

use crate::syntax::HighlightTheme;

/// Predefined theme choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    /// Atom-inspired One Dark Pro.
    OneDarkPro,
    /// Atom-inspired One Light.
    OneLight,
    /// Built-in default theme.
    Default,
}

impl Theme {
    /// Returns the theme configuration for this choice.
    pub fn get_theme(self) -> HighlightTheme {
        match self {
            Theme::OneDarkPro => HighlightTheme::one_dark_pro(),
            Theme::OneLight => HighlightTheme::one_light(),
            Theme::Default => HighlightTheme::default(),
        }
    }
}
