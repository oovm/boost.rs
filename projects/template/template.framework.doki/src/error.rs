use document_formats::FormatError;
use thiserror::Error;

use crate::diagnostic::Diagnostic;

/// Result alias for configuration loading and validation.
pub type ConfigResult<T> = Result<T, ConfigError>;
/// Result alias for session and build operations.
pub type CoreResult<T> = Result<T, CoreError>;

/// Configuration loading or validation failure.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Reading the config file failed at the filesystem level.
    #[error("failed to read project config")]
    Io {
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The TOML document could not be decoded into the config model.
    #[error("{message}")]
    Extract {
        /// Decode error detail.
        message: String,
    },

    /// The decoded config violates a contract rule.
    #[error("{message}")]
    Invalid {
        /// Validation error detail.
        message: String,
    },
}

impl ConfigError {
    /// Build an [`ConfigError::Invalid`] from a message.
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

/// Failure from a session operation, build step or markdown conversion.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Configuration loading or validation failed.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// Accessing project files failed at the filesystem level.
    #[error("failed to access project files")]
    Io {
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The session state rejected the operation.
    #[error("{message}")]
    Session {
        /// Reason the session operation failed.
        message: String,
    },

    /// A build step failed.
    #[error("{message}")]
    Build {
        /// Reason the build failed.
        message: String,
    },

    /// Importing markdown through the shared formats layer failed.
    #[error("markdown import failed")]
    MarkdownImport {
        /// Underlying format error.
        #[source]
        source: FormatError,
    },

    /// Exporting markdown through the shared formats layer failed.
    #[error("markdown export failed")]
    MarkdownExport {
        /// Underlying format error.
        #[source]
        source: FormatError,
    },
}

impl CoreError {
    /// Build a [`CoreError::Session`] from a message.
    pub fn session(message: impl Into<String>) -> Self {
        Self::Session { message: message.into() }
    }

    /// Build a [`CoreError::Build`] from a message.
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
