use document_formats::FormatError;
use thiserror::Error;

use crate::diagnostic::Diagnostic;

pub type ConfigResult<T> = Result<T, ConfigError>;
pub type CoreResult<T> = Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read project config")]
    Io {
        #[source]
        source: std::io::Error,
    },

    #[error("invalid project configuration")]
    Extract {
        #[source]
        source: figment::Error,
    },

    #[error("{message}")]
    Invalid { message: String },
}

impl ConfigError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid { message: message.into() }
    }

    /// Stable diagnostic code carried on the wire.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io { .. } => "doki::config::io",
            Self::Extract { .. } => "doki::config::extract",
            Self::Invalid { .. } => "doki::config::invalid",
        }
    }

    /// Recovery hint for [`Self::code`], when the variant has one.
    #[must_use]
    pub fn help(&self) -> Option<&'static str> {
        match self {
            Self::Io { .. } => Some("ensure doki.toml exists under the project root"),
            Self::Extract { .. } | Self::Invalid { .. } => None,
        }
    }

    /// Project the error onto the shared [`Diagnostic`] wire shape.
    #[must_use]
    pub fn to_diagnostic(&self) -> Diagnostic {
        diagnostic_for(self.code(), self.to_string(), self.help())
    }
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error("failed to access project files")]
    Io {
        #[source]
        source: std::io::Error,
    },

    #[error("{message}")]
    Session { message: String },

    #[error("{message}")]
    Build { message: String },

    #[error("markdown import failed")]
    MarkdownImport {
        #[source]
        source: FormatError,
    },

    #[error("markdown export failed")]
    MarkdownExport {
        #[source]
        source: FormatError,
    },
}

impl CoreError {
    pub fn session(message: impl Into<String>) -> Self {
        Self::Session { message: message.into() }
    }

    pub fn build(message: impl Into<String>) -> Self {
        Self::Build { message: message.into() }
    }

    /// Stable diagnostic code carried on the wire.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Config(error) => error.code(),
            Self::Io { .. } => "doki::io",
            Self::Session { .. } => "doki::session::invalid",
            Self::Build { .. } => "doki::build::invalid",
            Self::MarkdownImport { .. } => "doki::markdown::import",
            Self::MarkdownExport { .. } => "doki::markdown::export",
        }
    }

    /// Recovery hint for [`Self::code`], when the variant has one.
    #[must_use]
    pub fn help(&self) -> Option<&'static str> {
        match self {
            Self::Config(error) => error.help(),
            Self::Io { .. } | Self::Session { .. } | Self::Build { .. } | Self::MarkdownImport { .. } | Self::MarkdownExport { .. } => None,
        }
    }

    /// Project the error onto the shared [`Diagnostic`] wire shape.
    #[must_use]
    pub fn to_diagnostic(&self) -> Diagnostic {
        diagnostic_for(self.code(), self.to_string(), self.help())
    }
}

fn diagnostic_for(code: &'static str, message: String, help: Option<&'static str>) -> Diagnostic {
    let mut diagnostic = Diagnostic::error(code, message);
    diagnostic.help = help.map(str::to_owned);
    diagnostic
}
