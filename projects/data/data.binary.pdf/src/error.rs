//! Stable error surface of the byte-level PDF engine.
//!
//! Codes are contract: the product binding, the CLI and the localized copy
//! compare them, so a message may be reworded while its code stays. Codes are
//! also contract on their parameters: [`params`] freezes one payload shape per
//! code, so a binding forwards a typed object instead of re-deriving parameters
//! from prose, and a payload that does not match its code's shape is refused.

use std::fmt;

use serde::Serialize;

/// Stable codes reported by the PDF engine.
pub mod codes {
    /// The request itself was rejected, such as an empty page selection.
    pub const ARGUMENT_INVALID: &str = "session/argument-invalid";
    /// The input is not a readable PDF.
    pub const FORMAT_INVALID: &str = "format/invalid";
    /// The input uses a structure or filter this build does not support.
    pub const FORMAT_UNSUPPORTED: &str = "format/unsupported";
    /// Reading an input file failed.
    pub const IO_READ: &str = "io/read";
    /// Writing an output file failed.
    pub const IO_WRITE: &str = "io/write";
    /// The output exists and overwrite was not granted.
    pub const IO_EXISTS: &str = "io/exists";
}

/// Frozen parameter payloads, one shape per stable code.
///
/// [`ErrorParams::parse`] is the gate: a payload must be an object carrying
/// exactly the keys its code declares, with the declared types, so an
/// unstructured or over-filled payload is refused rather than forwarded.
pub mod params {
    use serde_json::{Map, Value};

    use super::codes;

    /// One code's frozen payload.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ErrorParams {
        /// `session/argument-invalid`; `pages` and `arg` narrow the rejected input.
        ArgumentInvalid {
            /// Why the request was refused.
            reason: String,
            /// The rejected page selection, when the failure is about pages.
            pages: Option<String>,
            /// The rejected argument name, when the failure names one.
            arg: Option<String>,
        },
        /// `format/invalid`; the input is not a readable PDF.
        Invalid {
            /// Why the input failed validation.
            reason: String,
        },
        /// `format/unsupported`; a structure or filter is not supported.
        Unsupported {
            /// Why the input is outside this build.
            reason: String,
        },
        /// `io/read`; reading an input file failed.
        Read {
            /// Path that could not be read.
            path: String,
            /// Operating-system error kind, such as `NotFound`.
            kind: String,
        },
        /// `io/write`; writing an output file failed.
        Write {
            /// Path that could not be written.
            path: String,
        },
        /// `io/exists`; the output is present and overwrite was not granted.
        Exists {
            /// Path that already exists.
            path: String,
        },
    }

    impl ErrorParams {
        /// Stable code this payload belongs to.
        pub fn code(&self) -> &'static str {
            match self {
                Self::ArgumentInvalid { .. } => codes::ARGUMENT_INVALID,
                Self::Invalid { .. } => codes::FORMAT_INVALID,
                Self::Unsupported { .. } => codes::FORMAT_UNSUPPORTED,
                Self::Read { .. } => codes::IO_READ,
                Self::Write { .. } => codes::IO_WRITE,
                Self::Exists { .. } => codes::IO_EXISTS,
            }
        }

        /// Serializes the payload with the frozen key names.
        pub fn to_json(&self) -> Value {
            match self {
                Self::ArgumentInvalid { reason, pages, arg } => {
                    let mut object = Map::new();
                    object.insert("reason".to_owned(), Value::from(reason.as_str()));
                    if let Some(pages) = pages {
                        object.insert("pages".to_owned(), Value::from(pages.as_str()));
                    }
                    if let Some(arg) = arg {
                        object.insert("arg".to_owned(), Value::from(arg.as_str()));
                    }
                    Value::Object(object)
                }
                Self::Invalid { reason } | Self::Unsupported { reason } => {
                    let mut object = Map::new();
                    object.insert("reason".to_owned(), Value::from(reason.as_str()));
                    Value::Object(object)
                }
                Self::Read { path, kind } => {
                    let mut object = Map::new();
                    object.insert("path".to_owned(), Value::from(path.as_str()));
                    object.insert("kind".to_owned(), Value::from(kind.as_str()));
                    Value::Object(object)
                }
                Self::Write { path } | Self::Exists { path } => {
                    let mut object = Map::new();
                    object.insert("path".to_owned(), Value::from(path.as_str()));
                    Value::Object(object)
                }
            }
        }

        /// Validates one wire payload against its code's frozen shape.
        ///
        /// The payload must be a JSON object with exactly the declared keys;
        /// missing, extra or mistyped values are refused.
        pub fn parse(code: &str, payload: &Value) -> Result<Self, ParamError> {
            let Some(object) = payload.as_object()
            else {
                return Err(ParamError::NotAnObject);
            };
            match code {
                codes::ARGUMENT_INVALID => {
                    allow_keys(object, &["reason", "pages", "arg"])?;
                    Ok(Self::ArgumentInvalid {
                        reason: required_str(object, "reason")?,
                        pages: optional_str(object, "pages")?,
                        arg: optional_str(object, "arg")?,
                    })
                }
                codes::FORMAT_INVALID => {
                    allow_keys(object, &["reason"])?;
                    Ok(Self::Invalid { reason: required_str(object, "reason")? })
                }
                codes::FORMAT_UNSUPPORTED => {
                    allow_keys(object, &["reason"])?;
                    Ok(Self::Unsupported { reason: required_str(object, "reason")? })
                }
                codes::IO_READ => {
                    allow_keys(object, &["path", "kind"])?;
                    Ok(Self::Read { path: required_str(object, "path")?, kind: required_str(object, "kind")? })
                }
                codes::IO_WRITE => {
                    allow_keys(object, &["path"])?;
                    Ok(Self::Write { path: required_str(object, "path")? })
                }
                codes::IO_EXISTS => {
                    allow_keys(object, &["path"])?;
                    Ok(Self::Exists { path: required_str(object, "path")? })
                }
                other => Err(ParamError::UnknownCode(other.to_owned())),
            }
        }
    }

    /// Why a payload was refused.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ParamError {
        /// The code has no frozen payload shape.
        UnknownCode(String),
        /// The payload is not a JSON object.
        NotAnObject,
        /// A declared key is absent.
        Missing(&'static str),
        /// A key outside the frozen shape was supplied.
        UnknownKey(String),
        /// A key carries the wrong JSON type.
        WrongType(&'static str),
        /// The error itself carries no data for its code's frozen shape.
        Unstructured(String),
    }

    fn allow_keys(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), ParamError> {
        if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(ParamError::UnknownKey(key.clone()));
        }
        Ok(())
    }

    fn required_str(object: &Map<String, Value>, key: &'static str) -> Result<String, ParamError> {
        match object.get(key) {
            None => Err(ParamError::Missing(key)),
            Some(value) => value.as_str().map(str::to_owned).ok_or(ParamError::WrongType(key)),
        }
    }

    fn optional_str(object: &Map<String, Value>, key: &'static str) -> Result<Option<String>, ParamError> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value.as_str().map(|text| Some(text.to_owned())).ok_or(ParamError::WrongType(key)),
        }
    }
}

pub use params::{ErrorParams, ParamError};

/// A failure reported by the PDF engine.
///
/// The code is part of the public contract and stays stable across message
/// changes, so callers compare codes instead of parsing text. The frozen
/// payload is attached at construction time, so bindings forward typed values
/// instead of re-deriving them from the message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PdfError {
    code: String,
    message: String,
    #[serde(skip)]
    params: Option<params::ErrorParams>,
}

impl PdfError {
    /// Builds an error from a stable code and a human-readable message.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), params: None }
    }

    /// Attaches the frozen payload of the error's code.
    #[must_use]
    pub fn with_params(mut self, params: params::ErrorParams) -> Self {
        self.params = Some(params);
        self
    }

    /// Returns the stable error code.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Returns the human-readable message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Frozen payload of the code, when the error carries the data its shape needs.
    pub fn params(&self) -> Option<&params::ErrorParams> {
        self.params.as_ref()
    }

    /// Frozen payload, refusing an error that cannot fill its own shape.
    pub fn frozen_params(&self) -> std::result::Result<params::ErrorParams, params::ParamError> {
        self.params.clone().ok_or_else(|| params::ParamError::Unstructured(self.code.clone()))
    }
}

impl fmt::Display for PdfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PdfError {}

/// Result alias used by every fallible engine entry point.
pub type Result<T> = std::result::Result<T, PdfError>;

/// Builds a `session/argument-invalid` error carrying `reason` as its payload.
pub fn argument(message: impl Into<String>) -> PdfError {
    let message = message.into();
    PdfError::new(codes::ARGUMENT_INVALID, message.clone()).with_params(params::ErrorParams::ArgumentInvalid {
        reason: message,
        pages: None,
        arg: None,
    })
}

/// Builds a `format/invalid` error carrying `reason` as its payload.
pub fn invalid(message: impl Into<String>) -> PdfError {
    let message = message.into();
    PdfError::new(codes::FORMAT_INVALID, message.clone()).with_params(params::ErrorParams::Invalid { reason: message })
}

/// Builds a `format/unsupported` error carrying `reason` as its payload.
pub fn unsupported(message: impl Into<String>) -> PdfError {
    let message = message.into();
    PdfError::new(codes::FORMAT_UNSUPPORTED, message.clone()).with_params(params::ErrorParams::Unsupported { reason: message })
}

/// Builds a message-only `io/read` error.
///
/// Callers that know the path use [`read_error_at`] instead, so the frozen
/// payload carries `path` and `kind` rather than the message having to.
pub fn read_error(message: impl Into<String>) -> PdfError {
    PdfError::new(codes::IO_READ, message)
}

/// Builds an `io/read` error carrying the path and OS error kind as its payload.
pub fn read_error_at(path: impl AsRef<std::path::Path>, kind: impl Into<String>, message: impl Into<String>) -> PdfError {
    PdfError::new(codes::IO_READ, message)
        .with_params(params::ErrorParams::Read { path: path.as_ref().display().to_string(), kind: kind.into() })
}

/// Builds a message-only `io/write` error.
pub fn write_error(message: impl Into<String>) -> PdfError {
    PdfError::new(codes::IO_WRITE, message)
}

/// Builds an `io/write` error carrying the path as its payload.
pub fn write_error_at(path: impl AsRef<std::path::Path>, message: impl Into<String>) -> PdfError {
    PdfError::new(codes::IO_WRITE, message).with_params(params::ErrorParams::Write { path: path.as_ref().display().to_string() })
}

/// Builds a message-only `io/exists` error.
pub fn exists_error(message: impl Into<String>) -> PdfError {
    PdfError::new(codes::IO_EXISTS, message)
}

/// Builds an `io/exists` error carrying the blocked path as its payload.
pub fn exists_error_at(path: impl AsRef<std::path::Path>, message: impl Into<String>) -> PdfError {
    PdfError::new(codes::IO_EXISTS, message).with_params(params::ErrorParams::Exists { path: path.as_ref().display().to_string() })
}
