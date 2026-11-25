//! Stable error surface of document authoring.
//!
//! Codes are contract: the TypeScript SDK, the CLI and the skill documents
//! compare them, so the message may be reworded while the code stays.
//!
//! Codes are also contract on their parameters. [`params`] freezes one payload
//! shape per code, so a binding forwards a typed object instead of re-deriving
//! parameters from prose, and a payload that does not match its code's shape is
//! refused instead of passing through half-formed.

/// Stable codes reported by document operations.
pub mod codes {
    /// A request parameter was rejected.
    pub const ARGUMENT_INVALID: &str = "session/argument-invalid";
    /// An operation exceeded the session budget.
    pub const BUDGET_EXCEEDED: &str = "session/budget-exceeded";
    /// No live document has the requested handle.
    pub const DOCUMENT_NOT_FOUND: &str = "document/not-found";
    /// The document moved past the revision the edit expected.
    pub const REVISION_CONFLICT: &str = "document/revision-conflict";
    /// An addressed block or other object is unknown.
    pub const OBJECT_NOT_FOUND: &str = "object/not-found";
    /// An edit operation or merge mode is not available in this build.
    pub const OBJECT_UNSUPPORTED: &str = "object/unsupported";
    /// An input or serialization failed format validation.
    pub const FORMAT_INVALID: &str = "format/invalid";
    /// A format has no route in this build.
    pub const FORMAT_UNSUPPORTED: &str = "format/unsupported";
    /// Reading a file failed.
    pub const IO_READ: &str = "io/read";
    /// Publishing a file failed.
    pub const IO_WRITE: &str = "io/write";
    /// The target exists and overwrite was not granted.
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

    /// Kind of object addressed by an `object/not-found` payload.
    ///
    /// The set is closed for document objects; a payload naming anything else
    /// is refused.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ObjectKind {
        /// One semantic block.
        Block,
    }

    impl ObjectKind {
        /// Wire name used by the contract.
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Block => "block",
            }
        }

        /// Parses a wire name, refusing anything outside the frozen set.
        pub fn parse(value: &str) -> Option<Self> {
            match value {
                "block" => Some(Self::Block),
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
        /// `session/budget-exceeded`; no parameters.
        BudgetExceeded,
        /// `document/not-found`.
        DocumentNotFound {
            /// Handle that has no live document, when one was addressed.
            handle: Option<u64>,
        },
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
            /// Identity of the missing object.
            id: String,
        },
        /// `object/unsupported`.
        Unsupported {
            /// Kind of capability that is unavailable, e.g. `edit operation`.
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
            /// Format this build does not route.
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
    }

    impl ErrorParams {
        /// Stable code this payload belongs to.
        pub fn code(&self) -> &'static str {
            match self {
                Self::ArgumentInvalid { .. } => codes::ARGUMENT_INVALID,
                Self::BudgetExceeded => codes::BUDGET_EXCEEDED,
                Self::DocumentNotFound { .. } => codes::DOCUMENT_NOT_FOUND,
                Self::RevisionConflict { .. } => codes::REVISION_CONFLICT,
                Self::NotFound { .. } => codes::OBJECT_NOT_FOUND,
                Self::Unsupported { .. } => codes::OBJECT_UNSUPPORTED,
                Self::FormatInvalid { .. } => codes::FORMAT_INVALID,
                Self::FormatUnsupported { .. } => codes::FORMAT_UNSUPPORTED,
                Self::IoRead { .. } => codes::IO_READ,
                Self::IoWrite { .. } => codes::IO_WRITE,
                Self::IoExists { .. } => codes::IO_EXISTS,
            }
        }

        /// Serializes the payload with the frozen key names.
        pub fn to_json(&self) -> Value {
            match self {
                Self::ArgumentInvalid { reason } => reason_object(reason),
                Self::BudgetExceeded => Value::Object(Map::new()),
                Self::DocumentNotFound { handle } => {
                    let mut object = Map::new();
                    if let Some(handle) = handle {
                        object.insert("handle".to_owned(), Value::from(*handle));
                    }
                    Value::Object(object)
                }
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
                    object.insert("id".to_owned(), Value::from(id.as_str()));
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
                codes::ARGUMENT_INVALID => {
                    allow_keys(object, &["reason"])?;
                    Ok(Self::ArgumentInvalid { reason: required_str(object, "reason")? })
                }
                codes::BUDGET_EXCEEDED => {
                    allow_keys(object, &[])?;
                    Ok(Self::BudgetExceeded)
                }
                codes::DOCUMENT_NOT_FOUND => {
                    allow_keys(object, &["handle"])?;
                    let handle = match object.get("handle") {
                        None | Some(Value::Null) => None,
                        Some(value) => Some(value.as_u64().ok_or(ParamError::WrongType("handle"))?),
                    };
                    Ok(Self::DocumentNotFound { handle })
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
                    Ok(Self::NotFound { kind: required_kind(object)?, id: required_str(object, "id")? })
                }
                codes::OBJECT_UNSUPPORTED => {
                    allow_keys(object, &["operation", "name"])?;
                    Ok(Self::Unsupported { operation: required_str(object, "operation")?, name: required_str(object, "name")? })
                }
                codes::FORMAT_INVALID => {
                    allow_keys(object, &["format", "reason"])?;
                    Ok(Self::FormatInvalid { format: required_str(object, "format")?, reason: required_str(object, "reason")? })
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

    fn reason_object(reason: &str) -> Value {
        let mut object = Map::new();
        object.insert("reason".to_owned(), Value::from(reason));
        Value::Object(object)
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
