use crate::source::SourceRef;

/// Result type for parsing operations.
pub type ParseResult<T> = Result<T, TextError>;

/// Diagnostic information for parsing operations.
///
/// Contains both the primary result and any non-fatal errors or warnings.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(bound(serialize = "T: serde::Serialize", deserialize = "T: serde::Deserialize<'de>")))]
pub struct TextDiagnostics<T> {
    /// The primary result of the parsing operation.
    /// May contain either a successful value or a fatal error.
    pub result: Result<T, TextError>,
    /// A collection of non-fatal errors or warnings encountered during the operation.
    pub diagnostics: Vec<TextError>,
}

impl<T: Clone> Clone for TextDiagnostics<T> {
    fn clone(&self) -> Self {
        Self { result: self.result.clone(), diagnostics: self.diagnostics.clone() }
    }
}

impl<T> TextDiagnostics<T> {
    /// Creates a new TextDiagnostics with the given result and no diagnostics.
    pub fn new(result: Result<T, TextError>) -> Self {
        Self { result, diagnostics: Vec::new() }
    }

    /// Creates a new TextDiagnostics with a successful result.
    pub fn success(value: T) -> Self {
        Self { result: Ok(value), diagnostics: Vec::new() }
    }

    /// Creates a new TextDiagnostics with a fatal error.
    pub fn error(error: TextError) -> Self {
        Self { result: Err(error), diagnostics: Vec::new() }
    }

    /// Returns true if there are any fatal errors or diagnostics.
    pub fn has_errors(&self) -> bool {
        self.result.is_err() || !self.diagnostics.is_empty()
    }
}

impl<'a, L: crate::Language> TextDiagnostics<&'a crate::tree::GreenNode<'a, L>> {
    /// Returns the successful green node result, panicking on error.
    pub fn green(&self) -> &'a crate::tree::GreenNode<'a, L> {
        self.result.as_ref().expect("Failed to get green node from parse output")
    }
}

/// The main error type for the text parsing framework.
///
/// `TextError` represents all possible language that can occur during
/// lexical analysis and parsing operations. It provides detailed
/// error information including error kind and precise source location.
#[derive(Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TextError {
    /// The specific kind of error.
    kind: Box<TextErrorKind>,
}

impl TextError {
    /// Creates a new TextError with the given kind.
    pub fn new(kind: TextErrorKind) -> Self {
        Self { kind: Box::new(kind) }
    }

    /// Byte offset in the parsed source when the diagnostic is location-bearing.
    pub fn source_offset(&self) -> Option<usize> {
        self.kind.source_offset()
    }

    /// Creates a new custom error with the given message.
    pub fn custom_error(message: impl Into<String>) -> Self {
        Self::new(TextErrorKind::CustomError { message: message.into() })
    }
}

impl From<TextErrorKind> for TextError {
    fn from(kind: TextErrorKind) -> Self {
        Self { kind: Box::new(kind) }
    }
}

impl std::fmt::Debug for TextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::fmt::Display for TextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.kind)
    }
}

impl std::error::Error for TextError {}

#[cfg(feature = "serde")]
impl serde::ser::Error for TextError {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        TextError::serde_error(msg.to_string())
    }
}

#[cfg(feature = "serde")]
impl serde::de::Error for TextError {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        TextError::deserialize_error(msg.to_string())
    }
}

#[cfg(feature = "serde")]
mod serde_io_error {
    pub fn serialize<S>(error: &std::io::Error, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serde::Serialize::serialize(&error.to_string(), serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<std::io::Error, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <String as serde::Deserialize>::deserialize(deserializer)?;
        Ok(std::io::Error::new(std::io::ErrorKind::Other, s))
    }
}

/// Enumeration of all possible error kinds in the text parsing framework.
///
/// This enum categorizes different types of language that can occur
/// during parsing operations, each with specific associated data
/// relevant to that error type.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum TextErrorKind {
    /// I/O error that occurred while reading source files.
    IoError {
        /// The underlying I/O error.
        #[cfg_attr(feature = "serde", serde(with = "crate::errors::serde_io_error"))]
        error: std::io::Error,
        /// Optional source reference of the file that caused the error.
        source_ref: Option<SourceRef>,
    },
    /// Syntax error encountered during parsing.
    SyntaxError {
        /// The error message.
        message: String,
        /// The byte offset where the error occurred.
        offset: usize,
        /// Optional source reference of the file that caused the error.
        source_ref: Option<SourceRef>,
    },
    /// Unexpected character encountered during lexical analysis.
    UnexpectedCharacter {
        /// The character that was not expected at this position.
        character: char,
        /// The byte offset where the unexpected character was found.
        offset: usize,
        /// Optional source reference of the file that caused the error.
        source_ref: Option<SourceRef>,
    },

    /// Unexpected token encountered during parsing.
    UnexpectedToken {
        /// The token that was not expected.
        token: String,
        /// The byte offset where the unexpected token was found.
        offset: usize,
        /// Optional source reference of the file that caused the error.
        source_ref: Option<SourceRef>,
    },

    /// Unexpected end of file encountered during parsing.
    UnexpectedEof {
        /// The byte offset where the EOF was encountered.
        offset: usize,
        /// Optional source reference of the file that caused the error.
        source_ref: Option<SourceRef>,
    },

    /// Custom error for user-defined error conditions.
    CustomError {
        /// The error message.
        message: String,
    },

    /// Invalid theme error for highlighting.
    InvalidTheme {
        /// The error message.
        message: String,
    },

    /// Unsupported format error for exporting.
    UnsupportedFormat {
        /// The unsupported format.
        format: String,
    },

    /// Color parsing error for themes.
    ColorParseError {
        /// The invalid color string.
        color: String,
    },

    /// Formatting error.
    FormatError {
        /// The error message.
        message: String,
    },

    /// Semantic error.
    SemanticError {
        /// The error message.
        message: String,
    },

    /// Protocol error (e.g., MCP, LSP).
    ProtocolError {
        /// The error message.
        message: String,
    },

    /// Expected a specific token.
    ExpectedToken {
        /// The token that was expected.
        expected: String,
        /// The byte offset where the error occurred.
        offset: usize,
        /// Optional source reference of the file that caused the error.
        source_ref: Option<SourceRef>,
    },

    /// Expected a name (identifier).
    ExpectedName {
        /// The kind of name that was expected (e.g., "function name").
        name_kind: String,
        /// The byte offset where the error occurred.
        offset: usize,
        /// Optional source reference of the file that caused the error.
        source_ref: Option<SourceRef>,
    },

    /// Trailing comma is not allowed.
    TrailingCommaNotAllowed {
        /// The byte offset where the error occurred.
        offset: usize,
        /// Optional source reference of the file that caused the error.
        source_ref: Option<SourceRef>,
    },

    /// Test failure error.
    TestFailure {
        /// The file that failed the test.
        path: std::path::PathBuf,
        /// The expected output.
        expected: String,
        /// The actual output.
        actual: String,
    },

    /// Test regenerated.
    TestRegenerated {
        /// The file that was regenerated.
        path: std::path::PathBuf,
    },

    /// Serde error.
    SerdeError {
        /// The error message.
        message: String,
    },

    /// Serde deserialization error.
    DeserializeError {
        /// The error message.
        message: String,
    },

    /// XML error.
    XmlError {
        /// The error message.
        message: String,
    },

    /// Zip error.
    ZipError {
        /// The error message.
        message: String,
    },

    /// Parse error.
    ParseError {
        /// The error message.
        message: String,
    },

    /// Internal error.
    InternalError {
        /// The error message.
        message: String,
    },
}

impl TextErrorKind {
    /// Gets the i18n key for this error kind.
    pub fn key(&self) -> &'static str {
        match self {
            TextErrorKind::IoError { .. } => "error.io",
            TextErrorKind::SyntaxError { .. } => "error.syntax",
            TextErrorKind::UnexpectedCharacter { .. } => "error.unexpected_character",
            TextErrorKind::UnexpectedToken { .. } => "error.unexpected_token",
            TextErrorKind::UnexpectedEof { .. } => "error.unexpected_eof",
            TextErrorKind::CustomError { .. } => "error.custom",
            TextErrorKind::InvalidTheme { .. } => "error.invalid_theme",
            TextErrorKind::UnsupportedFormat { .. } => "error.unsupported_format",
            TextErrorKind::ColorParseError { .. } => "error.color_parse",
            TextErrorKind::FormatError { .. } => "error.format",
            TextErrorKind::SemanticError { .. } => "error.semantic",
            TextErrorKind::ProtocolError { .. } => "error.protocol",
            TextErrorKind::ExpectedToken { .. } => "error.expected_token",
            TextErrorKind::ExpectedName { .. } => "error.expected_name",
            TextErrorKind::TrailingCommaNotAllowed { .. } => "error.trailing_comma_not_allowed",
            TextErrorKind::TestFailure { .. } => "error.test_failure",
            TextErrorKind::TestRegenerated { .. } => "error.test_regenerated",
            TextErrorKind::SerdeError { .. } => "error.serde",
            TextErrorKind::DeserializeError { .. } => "error.deserialize",
            TextErrorKind::XmlError { .. } => "error.xml",
            TextErrorKind::ZipError { .. } => "error.zip",
            TextErrorKind::ParseError { .. } => "error.parse",
            TextErrorKind::InternalError { .. } => "error.internal",
        }
    }

    /// Byte offset in the parsed source when this diagnostic is location-bearing.
    pub fn source_offset(&self) -> Option<usize> {
        match self {
            TextErrorKind::SyntaxError { offset, .. }
            | TextErrorKind::UnexpectedCharacter { offset, .. }
            | TextErrorKind::UnexpectedToken { offset, .. }
            | TextErrorKind::UnexpectedEof { offset, .. }
            | TextErrorKind::ExpectedToken { offset, .. }
            | TextErrorKind::ExpectedName { offset, .. }
            | TextErrorKind::TrailingCommaNotAllowed { offset, .. } => Some(*offset),
            _ => None,
        }
    }
}

impl std::fmt::Display for TextErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TextErrorKind::IoError { error, source_ref } => {
                if let Some(id) = source_ref {
                    write!(f, "I/O error in {}: {}", id, error)
                }
                else {
                    write!(f, "I/O error: {}", error)
                }
            }
            TextErrorKind::SyntaxError { message, offset, source_ref } => {
                if let Some(id) = source_ref {
                    write!(f, "Syntax error in {} at offset {}: {}", id, offset, message)
                }
                else {
                    write!(f, "Syntax error at offset {}: {}", offset, message)
                }
            }
            TextErrorKind::UnexpectedCharacter { character, offset, source_ref } => {
                if let Some(id) = source_ref {
                    write!(f, "Unexpected character '{}' in {} at offset {}", character, id, offset)
                }
                else {
                    write!(f, "Unexpected character '{}' at offset {}", character, offset)
                }
            }
            TextErrorKind::UnexpectedToken { token, offset, source_ref } => {
                if let Some(id) = source_ref {
                    write!(f, "Unexpected token '{}' in {} at offset {}", token, id, offset)
                }
                else {
                    write!(f, "Unexpected token '{}' at offset {}", token, offset)
                }
            }
            TextErrorKind::UnexpectedEof { offset, source_ref } => {
                if let Some(id) = source_ref {
                    write!(f, "Unexpected end of file in {} at offset {}", id, offset)
                }
                else {
                    write!(f, "Unexpected end of file at offset {}", offset)
                }
            }
            TextErrorKind::ExpectedToken { expected, offset, source_ref } => {
                if let Some(id) = source_ref {
                    write!(f, "Expected token '{}' in {} at offset {}", expected, id, offset)
                }
                else {
                    write!(f, "Expected token '{}' at offset {}", expected, offset)
                }
            }
            TextErrorKind::ExpectedName { name_kind, offset, source_ref } => {
                if let Some(id) = source_ref {
                    write!(f, "Expected {} in {} at offset {}", name_kind, id, offset)
                }
                else {
                    write!(f, "Expected {} at offset {}", name_kind, offset)
                }
            }
            TextErrorKind::TrailingCommaNotAllowed { offset, source_ref } => {
                if let Some(id) = source_ref {
                    write!(f, "Trailing comma not allowed in {} at offset {}", id, offset)
                }
                else {
                    write!(f, "Trailing comma not allowed at offset {}", offset)
                }
            }
            TextErrorKind::CustomError { message } => {
                write!(f, "Custom error: {}", message)
            }
            TextErrorKind::InvalidTheme { message } => {
                write!(f, "Invalid theme: {}", message)
            }
            TextErrorKind::UnsupportedFormat { format } => {
                write!(f, "Unsupported format: {}", format)
            }
            TextErrorKind::ColorParseError { color } => {
                write!(f, "Invalid color: {}", color)
            }
            TextErrorKind::FormatError { message } => {
                write!(f, "Format error: {}", message)
            }
            TextErrorKind::SemanticError { message } => {
                write!(f, "Semantic error: {}", message)
            }
            TextErrorKind::ProtocolError { message } => {
                write!(f, "Protocol error: {}", message)
            }
            TextErrorKind::TestFailure { path, expected, actual } => {
                write!(f, "Test failed for {}: expected '{}', got '{}'", path.display(), expected, actual)
            }
            TextErrorKind::TestRegenerated { path } => {
                write!(f, "Test regenerated for {}", path.display())
            }
            TextErrorKind::SerdeError { message } => {
                write!(f, "Serialization error: {}", message)
            }
            TextErrorKind::DeserializeError { message } => {
                write!(f, "Deserialization error: {}", message)
            }
            TextErrorKind::XmlError { message } => {
                write!(f, "XML error: {}", message)
            }
            TextErrorKind::ZipError { message } => {
                write!(f, "ZIP error: {}", message)
            }
            TextErrorKind::ParseError { message } => {
                write!(f, "Parse error: {}", message)
            }
            TextErrorKind::InternalError { message } => {
                write!(f, "Internal error: {}", message)
            }
        }
    }
}

impl TextError {
    /// Gets the kind of this error.
    pub fn kind(&self) -> &TextErrorKind {
        &self.kind
    }

    /// Creates a test failure error.
    pub fn test_failure(path: std::path::PathBuf, expected: String, actual: String) -> Self {
        TextErrorKind::TestFailure { path, expected, actual }.into()
    }

    /// Creates a test regenerated error.
    pub fn test_regenerated(path: std::path::PathBuf) -> Self {
        TextErrorKind::TestRegenerated { path }.into()
    }

    /// Creates an I/O error with an optional source reference.
    pub fn io_error(error: std::io::Error, source_ref: SourceRef) -> Self {
        TextErrorKind::IoError { error, source_ref: Some(source_ref) }.into()
    }

    /// Creates a syntax error with a message and location.
    pub fn syntax_error(message: impl Into<String>, offset: usize, source_ref: Option<SourceRef>) -> Self {
        TextErrorKind::SyntaxError { message: message.into(), offset, source_ref }.into()
    }

    /// Creates an unexpected character error.
    pub fn unexpected_character(character: char, offset: usize, source_ref: Option<SourceRef>) -> Self {
        TextErrorKind::UnexpectedCharacter { character, offset, source_ref }.into()
    }

    /// Creates an unexpected token error.
    pub fn unexpected_token(token: impl Into<String>, offset: usize, source_ref: Option<SourceRef>) -> Self {
        TextErrorKind::UnexpectedToken { token: token.into(), offset, source_ref }.into()
    }

    /// Creates an unexpected end of file error.
    pub fn unexpected_eof(offset: usize, source_ref: Option<SourceRef>) -> Self {
        TextErrorKind::UnexpectedEof { offset, source_ref }.into()
    }

    /// Creates an expected token error.
    pub fn expected_token(expected: impl Into<String>, offset: usize, source_ref: Option<SourceRef>) -> Self {
        TextErrorKind::ExpectedToken { expected: expected.into(), offset, source_ref }.into()
    }

    /// Creates an expected name error.
    pub fn expected_name(name_kind: impl Into<String>, offset: usize, source_ref: Option<SourceRef>) -> Self {
        TextErrorKind::ExpectedName { name_kind: name_kind.into(), offset, source_ref }.into()
    }

    /// Creates a trailing comma not allowed error.
    pub fn trailing_comma_not_allowed(offset: usize, source_ref: Option<SourceRef>) -> Self {
        TextErrorKind::TrailingCommaNotAllowed { offset, source_ref }.into()
    }

    /// Creates an invalid theme error.
    pub fn invalid_theme(message: impl Into<String>) -> Self {
        TextErrorKind::InvalidTheme { message: message.into() }.into()
    }

    /// Creates an unsupported format error.
    pub fn unsupported_format(format: impl Into<String>) -> Self {
        TextErrorKind::UnsupportedFormat { format: format.into() }.into()
    }

    /// Creates a color parsing error.
    pub fn color_parse_error(color: impl Into<String>) -> Self {
        TextErrorKind::ColorParseError { color: color.into() }.into()
    }

    /// Creates a formatting error.
    pub fn format_error(message: impl Into<String>) -> Self {
        TextErrorKind::FormatError { message: message.into() }.into()
    }

    /// Creates a semantic error.
    pub fn semantic_error(message: impl Into<String>) -> Self {
        TextErrorKind::SemanticError { message: message.into() }.into()
    }

    /// Creates a protocol error.
    pub fn protocol_error(message: impl Into<String>) -> Self {
        TextErrorKind::ProtocolError { message: message.into() }.into()
    }

    /// Creates a serde error.
    pub fn serde_error(message: impl Into<String>) -> Self {
        TextErrorKind::SerdeError { message: message.into() }.into()
    }

    /// Creates a serde deserialization error.
    pub fn deserialize_error(message: impl Into<String>) -> Self {
        TextErrorKind::DeserializeError { message: message.into() }.into()
    }

    /// Creates an XML error.
    pub fn xml_error(message: impl Into<String>) -> Self {
        TextErrorKind::XmlError { message: message.into() }.into()
    }

    /// Creates a zip error.
    pub fn zip_error(message: impl Into<String>) -> Self {
        TextErrorKind::ZipError { message: message.into() }.into()
    }

    /// Creates a parse error.
    pub fn parse_error(message: impl Into<String>) -> Self {
        TextErrorKind::ParseError { message: message.into() }.into()
    }

    /// Creates an internal error.
    pub fn internal_error(message: impl Into<String>) -> Self {
        TextErrorKind::InternalError { message: message.into() }.into()
    }

    /// Attach a source reference to the error context.
    pub fn with_source_ref(mut self, source_ref: SourceRef) -> Self {
        match self.kind.as_mut() {
            TextErrorKind::IoError { source_ref: u, .. } => *u = Some(source_ref),
            TextErrorKind::SyntaxError { source_ref: u, .. } => *u = Some(source_ref),
            TextErrorKind::UnexpectedCharacter { source_ref: u, .. } => *u = Some(source_ref),
            TextErrorKind::UnexpectedToken { source_ref: u, .. } => *u = Some(source_ref),
            TextErrorKind::ExpectedToken { source_ref: u, .. } => *u = Some(source_ref),
            TextErrorKind::ExpectedName { source_ref: u, .. } => *u = Some(source_ref),
            TextErrorKind::TrailingCommaNotAllowed { source_ref: u, .. } => *u = Some(source_ref),
            _ => {}
        }
        self
    }
}

impl Clone for TextErrorKind {
    fn clone(&self) -> Self {
        match self {
            TextErrorKind::IoError { error, source_ref } => {
                // Since std::io::Error doesn't support Clone, we create a new error
                let new_error = std::io::Error::new(error.kind(), error.to_string());
                TextErrorKind::IoError { error: new_error, source_ref: source_ref.clone() }
            }
            TextErrorKind::SyntaxError { message, offset, source_ref } => {
                TextErrorKind::SyntaxError { message: message.clone(), offset: *offset, source_ref: source_ref.clone() }
            }
            TextErrorKind::UnexpectedCharacter { character, offset, source_ref } => {
                TextErrorKind::UnexpectedCharacter { character: *character, offset: *offset, source_ref: source_ref.clone() }
            }
            TextErrorKind::UnexpectedToken { token, offset, source_ref } => {
                TextErrorKind::UnexpectedToken { token: token.clone(), offset: *offset, source_ref: source_ref.clone() }
            }
            TextErrorKind::UnexpectedEof { offset, source_ref } => {
                TextErrorKind::UnexpectedEof { offset: *offset, source_ref: source_ref.clone() }
            }
            TextErrorKind::ExpectedToken { expected, offset, source_ref } => {
                TextErrorKind::ExpectedToken { expected: expected.clone(), offset: *offset, source_ref: source_ref.clone() }
            }
            TextErrorKind::ExpectedName { name_kind, offset, source_ref } => {
                TextErrorKind::ExpectedName { name_kind: name_kind.clone(), offset: *offset, source_ref: source_ref.clone() }
            }
            TextErrorKind::TrailingCommaNotAllowed { offset, source_ref } => {
                TextErrorKind::TrailingCommaNotAllowed { offset: *offset, source_ref: source_ref.clone() }
            }
            TextErrorKind::CustomError { message } => TextErrorKind::CustomError { message: message.clone() },
            TextErrorKind::InvalidTheme { message } => TextErrorKind::InvalidTheme { message: message.clone() },
            TextErrorKind::UnsupportedFormat { format } => TextErrorKind::UnsupportedFormat { format: format.clone() },
            TextErrorKind::ColorParseError { color } => TextErrorKind::ColorParseError { color: color.clone() },
            TextErrorKind::FormatError { message } => TextErrorKind::FormatError { message: message.clone() },
            TextErrorKind::SemanticError { message } => TextErrorKind::SemanticError { message: message.clone() },
            TextErrorKind::ProtocolError { message } => TextErrorKind::ProtocolError { message: message.clone() },
            TextErrorKind::TestFailure { path, expected, actual } => {
                TextErrorKind::TestFailure { path: path.clone(), expected: expected.clone(), actual: actual.clone() }
            }
            TextErrorKind::TestRegenerated { path } => TextErrorKind::TestRegenerated { path: path.clone() },
            TextErrorKind::SerdeError { message } => TextErrorKind::SerdeError { message: message.clone() },
            TextErrorKind::DeserializeError { message } => TextErrorKind::DeserializeError { message: message.clone() },
            TextErrorKind::XmlError { message } => TextErrorKind::XmlError { message: message.clone() },
            TextErrorKind::ZipError { message } => TextErrorKind::ZipError { message: message.clone() },
            TextErrorKind::ParseError { message } => TextErrorKind::ParseError { message: message.clone() },
            TextErrorKind::InternalError { message } => TextErrorKind::InternalError { message: message.clone() },
        }
    }
}
