//! Stable error surface of spreadsheet workbooks.
//!
//! Codes are contract: the TypeScript SDK, the CLI and the skill documents
//! compare them, so the message may be reworded while the code stays.
//!
//! Codes are also contract on their parameters. [`params`] freezes one payload
//! shape per code, so a binding forwards a typed object instead of re-deriving
//! parameters from prose, and a payload that does not match its code's shape is
//! refused instead of passing through half-formed.

/// Stable codes reported by workbook operations.
pub mod codes {
    /// A request parameter was rejected.
    pub const ARGUMENT_INVALID: &str = "session/argument-invalid";
    /// No workbook is open for the handle.
    pub const WORKBOOK_NOT_FOUND: &str = "workbook/not-found";
    /// The workbook moved past the revision the edit expected.
    pub const REVISION_CONFLICT: &str = "document/revision-conflict";
    /// An addressed sheet, defined name or other object is unknown.
    pub const OBJECT_NOT_FOUND: &str = "object/not-found";
    /// A template or edit operation is not available in this build.
    pub const OBJECT_UNSUPPORTED: &str = "object/unsupported";
    /// An input failed validation for the claimed format.
    pub const FORMAT_INVALID: &str = "format/invalid";
    /// A format is not offered by this build.
    pub const FORMAT_UNSUPPORTED: &str = "format/unsupported";
    /// Reading a file failed.
    pub const IO_READ: &str = "io/read";
    /// Publishing a file failed.
    pub const IO_WRITE: &str = "io/write";
    /// The target exists and overwrite was not granted.
    pub const IO_EXISTS: &str = "io/exists";
    /// An engine-level failure with no OS error behind it.
    pub const IO_FAILURE: &str = "io/failure";
}

/// Frozen parameter payloads, one shape per stable code.
///
/// [`ErrorParams::parse`] is the gate: a payload must be an object carrying
/// exactly the keys its code declares, with the declared types, so an
/// unstructured or over-filled payload is refused rather than forwarded.
pub mod params {
    use serde_json::{Map, Value};

    use super::codes;

    /// Kind of object addressed by an `object/not-found` payload.
    ///
    /// The set is closed for workbook objects; a payload naming anything else
    /// is refused.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ObjectKind {
        /// One sheet of the workbook.
        Sheet,
        /// One defined name.
        DefinedName,
    }

    impl ObjectKind {
        /// Wire name used by the contract.
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Sheet => "sheet",
                Self::DefinedName => "defined name",
            }
        }

        /// Parses a wire name, refusing anything outside the frozen set.
        pub fn parse(value: &str) -> Option<Self> {
            match value {
                "sheet" => Some(Self::Sheet),
                "defined name" => Some(Self::DefinedName),
                _ => None,
            }
        }
    }

    /// One code's frozen payload.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ErrorParams {
        /// `session/argument-invalid`.
        ArgumentInvalid {
            /// Why the argument was refused.
            reason: String,
        },
        /// `workbook/not-found`; no parameters.
        WorkbookNotFound,
        /// `document/revision-conflict`.
        RevisionConflict {
            /// Committed revision the edit was applied against.
            actual: u64,
            /// Revision the caller declared, when one was supplied.
            expected: Option<u64>,
        },
        /// `object/not-found`.
        NotFound {
            /// Kind of the missing object.
            kind: ObjectKind,
            /// Identity of the missing object, when one is known.
            id: Option<String>,
        },
        /// `object/unsupported`.
        Unsupported {
            /// Kind of capability that is unavailable, e.g. `template`.
            operation: String,
            /// Name of the unavailable capability.
            name: String,
        },
        /// `format/invalid`.
        FormatInvalid {
            /// Format the failed input claimed.
            format: String,
            /// Why the input was refused.
            reason: String,
        },
        /// `format/unsupported`.
        FormatUnsupported {
            /// Format this build does not offer.
            format: String,
        },
        /// `io/read`.
        IoRead {
            /// Path that could not be read.
            path: String,
            /// OS error kind.
            kind: String,
        },
        /// `io/write`.
        IoWrite {
            /// Path that could not be published.
            path: String,
        },
        /// `io/exists`.
        IoExists {
            /// Path that already exists.
            path: String,
        },
        /// `io/failure`.
        IoFailure {
            /// Why the engine-level operation failed.
            reason: String,
        },
    }

    impl ErrorParams {
        /// Stable code this payload belongs to.
        pub fn code(&self) -> &'static str {
            match self {
                Self::ArgumentInvalid { .. } => codes::ARGUMENT_INVALID,
                Self::WorkbookNotFound => codes::WORKBOOK_NOT_FOUND,
                Self::RevisionConflict { .. } => codes::REVISION_CONFLICT,
                Self::NotFound { .. } => codes::OBJECT_NOT_FOUND,
                Self::Unsupported { .. } => codes::OBJECT_UNSUPPORTED,
                Self::FormatInvalid { .. } => codes::FORMAT_INVALID,
                Self::FormatUnsupported { .. } => codes::FORMAT_UNSUPPORTED,
                Self::IoRead { .. } => codes::IO_READ,
                Self::IoWrite { .. } => codes::IO_WRITE,
                Self::IoExists { .. } => codes::IO_EXISTS,
                Self::IoFailure { .. } => codes::IO_FAILURE,
            }
        }

        /// Serializes the payload with the frozen key names.
        pub fn to_json(&self) -> Value {
            match self {
                Self::ArgumentInvalid { reason } | Self::IoFailure { reason } => {
                    let mut object = Map::new();
                    object.insert("reason".to_owned(), Value::from(reason.as_str()));
                    Value::Object(object)
                }
                Self::WorkbookNotFound => Value::Object(Map::new()),
                Self::RevisionConflict { actual, expected } => {
                    let mut object = Map::new();
                    object.insert("actual".to_owned(), Value::from(*actual));
                    if let Some(expected) = expected {
                        object.insert("expected".to_owned(), Value::from(*expected));
                    }
                    Value::Object(object)
                }
                Self::NotFound { kind, id } => {
                    let mut object = Map::new();
                    object.insert("kind".to_owned(), Value::from(kind.as_str()));
                    if let Some(id) = id {
                        object.insert("id".to_owned(), Value::from(id.as_str()));
                    }
                    Value::Object(object)
                }
                Self::Unsupported { operation, name } => {
                    let mut object = Map::new();
                    object.insert("operation".to_owned(), Value::from(operation.as_str()));
                    object.insert("name".to_owned(), Value::from(name.as_str()));
                    Value::Object(object)
                }
                Self::FormatInvalid { format, reason } => {
                    let mut object = Map::new();
                    object.insert("format".to_owned(), Value::from(format.as_str()));
                    object.insert("reason".to_owned(), Value::from(reason.as_str()));
                    Value::Object(object)
                }
                Self::FormatUnsupported { format } => {
                    let mut object = Map::new();
                    object.insert("format".to_owned(), Value::from(format.as_str()));
                    Value::Object(object)
                }
                Self::IoRead { path, kind } => {
                    let mut object = Map::new();
                    object.insert("path".to_owned(), Value::from(path.as_str()));
                    object.insert("kind".to_owned(), Value::from(kind.as_str()));
                    Value::Object(object)
                }
                Self::IoWrite { path } | Self::IoExists { path } => {
                    let mut object = Map::new();
                    object.insert("path".to_owned(), Value::from(path.as_str()));
                    Value::Object(object)
                }
            }
        }

        /// Validates one wire payload against its code's frozen shape.
        ///
        /// The payload must be a JSON object with exactly the declared keys;
        /// missing, extra, mistyped or unknown-kind values are refused.
        pub fn parse(code: &str, payload: &Value) -> Result<Self, ParamError> {
            let Some(object) = payload.as_object()
            else {
                return Err(ParamError::NotAnObject);
            };
            match code {
                codes::ARGUMENT_INVALID | codes::IO_FAILURE => {
                    allow_keys(object, &["reason"])?;
                    Ok(if code == codes::ARGUMENT_INVALID {
                        Self::ArgumentInvalid { reason: required_str(object, "reason")? }
                    }
                    else {
                        Self::IoFailure { reason: required_str(object, "reason")? }
                    })
                }
                codes::WORKBOOK_NOT_FOUND => {
                    allow_keys(object, &[])?;
                    Ok(Self::WorkbookNotFound)
                }
                codes::REVISION_CONFLICT => {
                    allow_keys(object, &["actual", "expected"])?;
                    let actual = required_u64(object, "actual")?;
                    let expected = match object.get("expected") {
                        None | Some(Value::Null) => None,
                        Some(value) => Some(value.as_u64().ok_or(ParamError::WrongType("expected"))?),
                    };
                    Ok(Self::RevisionConflict { actual, expected })
                }
                codes::OBJECT_NOT_FOUND => {
                    allow_keys(object, &["kind", "id"])?;
                    let kind = required_kind(object)?;
                    let id = match object.get("id") {
                        None | Some(Value::Null) => None,
                        Some(value) => Some(value.as_str().map(str::to_owned).ok_or(ParamError::WrongType("id"))?),
                    };
                    Ok(Self::NotFound { kind, id })
                }
                codes::OBJECT_UNSUPPORTED => {
                    allow_keys(object, &["operation", "name"])?;
                    Ok(Self::Unsupported {
                        operation: required_str(object, "operation")?,
                        name: required_str(object, "name")?,
                    })
                }
                codes::FORMAT_INVALID => {
                    allow_keys(object, &["format", "reason"])?;
                    Ok(Self::FormatInvalid {
                        format: required_str(object, "format")?,
                        reason: required_str(object, "reason")?,
                    })
                }
                codes::FORMAT_UNSUPPORTED => {
                    allow_keys(object, &["format"])?;
                    Ok(Self::FormatUnsupported { format: required_str(object, "format")? })
                }
                codes::IO_READ => {
                    allow_keys(object, &["path", "kind"])?;
                    Ok(Self::IoRead { path: required_str(object, "path")?, kind: required_str(object, "kind")? })
                }
                codes::IO_WRITE => {
                    allow_keys(object, &["path"])?;
                    Ok(Self::IoWrite { path: required_str(object, "path")? })
                }
                codes::IO_EXISTS => {
                    allow_keys(object, &["path"])?;
                    Ok(Self::IoExists { path: required_str(object, "path")? })
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
        /// A declared key is absent or carries the wrong JSON type.
        Missing(&'static str),
        /// A key outside the frozen shape was supplied.
        UnknownKey(String),
        /// An optional key carries the wrong JSON type.
        WrongType(&'static str),
        /// An `object/not-found` payload names a kind outside the frozen set.
        UnknownKind(String),
    }

    fn allow_keys(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), ParamError> {
        if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(ParamError::UnknownKey(key.clone()));
        }
        Ok(())
    }

    fn required_str(object: &Map<String, Value>, key: &'static str) -> Result<String, ParamError> {
        object.get(key).and_then(Value::as_str).map(str::to_owned).ok_or(ParamError::Missing(key))
    }

    fn required_u64(object: &Map<String, Value>, key: &'static str) -> Result<u64, ParamError> {
        object.get(key).and_then(Value::as_u64).ok_or(ParamError::Missing(key))
    }

    fn required_kind(object: &Map<String, Value>) -> Result<ObjectKind, ParamError> {
        let raw = required_str(object, "kind")?;
        ObjectKind::parse(&raw).ok_or(ParamError::UnknownKind(raw))
    }
}
