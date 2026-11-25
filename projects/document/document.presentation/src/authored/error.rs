//! Stable error surface of the authored deck model.
//!
//! Codes are contract: the TypeScript authoring surface, the CLI and the skill
//! documents compare them, so the message may be reworded while the code stays.
//!
//! Codes are also contract on their parameters. [`params`] freezes one payload
//! shape per code, so a binding forwards a typed object instead of re-deriving
//! parameters from prose, and a payload that does not match its code's shape is
//! refused instead of passing through half-formed.

use super::id::{ElementId, SlideId};

/// Stable codes reported by authored deck operations.
pub mod codes {
    /// The deck handle is gone or was never created.
    pub const DECK_NOT_FOUND: &str = "deck/not-found";
    /// `expected_revision` did not match the committed revision.
    pub const DECK_REVISION_CONFLICT: &str = "deck/revision-conflict";
    /// A slide, element, layout, master, theme or asset identity is unknown.
    pub const OBJECT_NOT_FOUND: &str = "object/not-found";
    /// A patch does not match the addressed element type.
    pub const OBJECT_TYPE_MISMATCH: &str = "object/type-mismatch";
    /// The object type exists in the format but is not editable yet.
    pub const OBJECT_UNSUPPORTED: &str = "object/unsupported";
    /// A referenced asset has no bytes or record.
    pub const ASSET_MISSING: &str = "asset/missing";
    /// An asset still placed by an element cannot be removed.
    pub const ASSET_IN_USE: &str = "asset/in-use";
    /// An argument is unusable, such as an empty identity or a negative size.
    pub const ARGUMENT_INVALID: &str = "session/argument-invalid";
    /// A native project container is malformed or fails its own checks.
    pub const FORMAT_INVALID_PACKAGE: &str = "format/invalid-package";
    /// A format or container version is not supported by this build.
    pub const FORMAT_UNSUPPORTED: &str = "format/unsupported";
    /// The playback session was asked to do something illegal in its state.
    pub const PLAYBACK_INVALID_STATE: &str = "playback/invalid-state";
}

/// Frozen parameter payloads, one shape per stable code.
///
/// [`ErrorParams::parse`] is the gate: a payload must be an object carrying
/// exactly the keys its code declares, with the declared types, so an
/// unstructured or over-filled payload is refused rather than forwarded.
pub mod params {
    use serde_json::{Map, Value};

    use super::codes;

    /// Kind of object addressed by a location-bearing code.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ObjectKind {
        /// The deck itself.
        Deck,
        /// One slide.
        Slide,
        /// One element inside a slide.
        Element,
        /// One slide layout.
        Layout,
        /// One master.
        Master,
        /// One theme.
        Theme,
        /// One embedded asset.
        Asset,
    }

    impl ObjectKind {
        /// Wire name used by the contract.
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Deck => "deck",
                Self::Slide => "slide",
                Self::Element => "element",
                Self::Layout => "layout",
                Self::Master => "master",
                Self::Theme => "theme",
                Self::Asset => "asset",
            }
        }

        /// Parses a wire name, refusing anything outside the frozen set.
        pub fn parse(value: &str) -> Option<Self> {
            match value {
                "deck" => Some(Self::Deck),
                "slide" => Some(Self::Slide),
                "element" => Some(Self::Element),
                "layout" => Some(Self::Layout),
                "master" => Some(Self::Master),
                "theme" => Some(Self::Theme),
                "asset" => Some(Self::Asset),
                _ => None,
            }
        }
    }

    /// One code's frozen payload.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ErrorParams {
        /// `deck/not-found`; no parameters.
        DeckNotFound,
        /// `deck/revision-conflict`.
        RevisionConflict {
            /// Committed revision the edit was applied against.
            actual: u64,
            /// Revision the edit script declared, when the caller supplied one.
            expected: Option<u64>,
        },
        /// `object/not-found`.
        NotFound {
            /// Kind of the missing object.
            kind: ObjectKind,
            /// Identity of the missing object.
            id: String,
        },
        /// `object/type-mismatch`.
        TypeMismatch {
            /// Kind of the addressed object.
            kind: ObjectKind,
            /// Identity of the addressed object.
            id: String,
        },
        /// `object/unsupported`.
        Unsupported {
            /// Capability or object type that is not editable.
            feature: String,
        },
        /// `asset/missing`.
        AssetMissing {
            /// Identity of the absent asset.
            asset_id: String,
        },
        /// `asset/in-use`.
        AssetInUse {
            /// Identity of the asset still placed by an element.
            asset_id: String,
        },
        /// `session/argument-invalid`.
        ArgumentInvalid {
            /// Why the argument was refused.
            reason: String,
        },
        /// `format/invalid-package`.
        InvalidPackage {
            /// Why the container failed validation.
            reason: String,
        },
        /// `format/unsupported`.
        UnsupportedFormat {
            /// Format or container name the build does not support.
            format: String,
        },
        /// `playback/invalid-state`.
        PlaybackInvalidState {
            /// Why the requested transition is illegal.
            reason: String,
        },
    }

    impl ErrorParams {
        /// Stable code this payload belongs to.
        pub fn code(&self) -> &'static str {
            match self {
                Self::DeckNotFound => codes::DECK_NOT_FOUND,
                Self::RevisionConflict { .. } => codes::DECK_REVISION_CONFLICT,
                Self::NotFound { .. } => codes::OBJECT_NOT_FOUND,
                Self::TypeMismatch { .. } => codes::OBJECT_TYPE_MISMATCH,
                Self::Unsupported { .. } => codes::OBJECT_UNSUPPORTED,
                Self::AssetMissing { .. } => codes::ASSET_MISSING,
                Self::AssetInUse { .. } => codes::ASSET_IN_USE,
                Self::ArgumentInvalid { .. } => codes::ARGUMENT_INVALID,
                Self::InvalidPackage { .. } => codes::FORMAT_INVALID_PACKAGE,
                Self::UnsupportedFormat { .. } => codes::FORMAT_UNSUPPORTED,
                Self::PlaybackInvalidState { .. } => codes::PLAYBACK_INVALID_STATE,
            }
        }

        /// Serializes the payload with the frozen key names.
        pub fn to_json(&self) -> Value {
            match self {
                Self::DeckNotFound => Value::Object(Map::new()),
                Self::RevisionConflict { actual, expected } => {
                    let mut object = Map::new();
                    object.insert("actual".to_owned(), Value::from(*actual));
                    if let Some(expected) = expected {
                        object.insert("expected".to_owned(), Value::from(*expected));
                    }
                    Value::Object(object)
                }
                Self::NotFound { kind, id } | Self::TypeMismatch { kind, id } => {
                    let mut object = Map::new();
                    object.insert("kind".to_owned(), Value::from(kind.as_str()));
                    object.insert("id".to_owned(), Value::from(id.as_str()));
                    Value::Object(object)
                }
                Self::Unsupported { feature } => {
                    let mut object = Map::new();
                    object.insert("feature".to_owned(), Value::from(feature.as_str()));
                    Value::Object(object)
                }
                Self::AssetMissing { asset_id } | Self::AssetInUse { asset_id } => {
                    let mut object = Map::new();
                    object.insert("assetId".to_owned(), Value::from(asset_id.as_str()));
                    Value::Object(object)
                }
                Self::ArgumentInvalid { reason }
                | Self::InvalidPackage { reason }
                | Self::PlaybackInvalidState { reason } => {
                    let mut object = Map::new();
                    object.insert("reason".to_owned(), Value::from(reason.as_str()));
                    Value::Object(object)
                }
                Self::UnsupportedFormat { format } => {
                    let mut object = Map::new();
                    object.insert("format".to_owned(), Value::from(format.as_str()));
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
                codes::DECK_NOT_FOUND => {
                    allow_keys(object, &[])?;
                    Ok(Self::DeckNotFound)
                }
                codes::DECK_REVISION_CONFLICT => {
                    allow_keys(object, &["actual", "expected"])?;
                    let actual = required_u64(object, "actual")?;
                    let expected = match object.get("expected") {
                        None | Some(Value::Null) => None,
                        Some(value) => Some(value.as_u64().ok_or(ParamError::WrongType("expected"))?),
                    };
                    Ok(Self::RevisionConflict { actual, expected })
                }
                codes::OBJECT_NOT_FOUND | codes::OBJECT_TYPE_MISMATCH => {
                    allow_keys(object, &["kind", "id"])?;
                    let kind = required_kind(object)?;
                    let id = required_str(object, "id")?;
                    Ok(if code == codes::OBJECT_NOT_FOUND {
                        Self::NotFound { kind, id }
                    }
                    else {
                        Self::TypeMismatch { kind, id }
                    })
                }
                codes::OBJECT_UNSUPPORTED => {
                    allow_keys(object, &["feature"])?;
                    Ok(Self::Unsupported { feature: required_str(object, "feature")? })
                }
                codes::ASSET_MISSING | codes::ASSET_IN_USE => {
                    allow_keys(object, &["assetId"])?;
                    let asset_id = required_str(object, "assetId")?;
                    Ok(if code == codes::ASSET_MISSING {
                        Self::AssetMissing { asset_id }
                    }
                    else {
                        Self::AssetInUse { asset_id }
                    })
                }
                codes::ARGUMENT_INVALID | codes::FORMAT_INVALID_PACKAGE | codes::PLAYBACK_INVALID_STATE => {
                    allow_keys(object, &["reason"])?;
                    let reason = required_str(object, "reason")?;
                    Ok(match code {
                        codes::ARGUMENT_INVALID => Self::ArgumentInvalid { reason },
                        codes::FORMAT_INVALID_PACKAGE => Self::InvalidPackage { reason },
                        _ => Self::PlaybackInvalidState { reason },
                    })
                }
                codes::FORMAT_UNSUPPORTED => {
                    allow_keys(object, &["format"])?;
                    Ok(Self::UnsupportedFormat { format: required_str(object, "format")? })
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
        /// An `object/*` payload names a kind outside the frozen set.
        UnknownKind(String),
        /// The error itself carries no data for its code's frozen shape.
        Unstructured(&'static str),
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

/// One authored-model failure with its locators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeckError {
    code: &'static str,
    message: String,
    revision: Option<u64>,
    expected: Option<u64>,
    slide_id: Option<SlideId>,
    element_id: Option<ElementId>,
    object: Option<(params::ObjectKind, String)>,
    asset: Option<String>,
    format: Option<String>,
    feature: Option<String>,
    recoverable: bool,
}

impl DeckError {
    /// Creates an error from a stable code and a message.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            revision: None,
            expected: None,
            slide_id: None,
            element_id: None,
            object: None,
            asset: None,
            format: None,
            feature: None,
            recoverable: false,
        }
    }

    /// `deck/not-found`: the handle is gone or was never created.
    pub fn deck_not_found() -> Self {
        Self::new(codes::DECK_NOT_FOUND, "the deck handle is gone or was never created")
    }

    /// `deck/revision-conflict`: the committed revision differs from the requested one.
    pub fn revision_conflict(actual: u64, expected: Option<u64>) -> Self {
        let message = match expected {
            Some(expected) => format!("the deck is at revision {actual}, not the expected revision {expected}"),
            None => format!("the deck is at revision {actual}"),
        };
        let mut error = Self::new(codes::DECK_REVISION_CONFLICT, message).with_revision(actual);
        error.expected = expected;
        error
    }

    /// `object/not-found`: one object identity is unknown.
    pub fn unknown(kind: params::ObjectKind, id: impl Into<String>) -> Self {
        let id = id.into();
        let mut error = Self::new(codes::OBJECT_NOT_FOUND, format!("{} `{id}` does not exist", kind.as_str()));
        error.object = Some((kind, id));
        error
    }

    /// `object/type-mismatch`: the addressed object does not accept the patch.
    pub fn mismatch(kind: params::ObjectKind, id: impl Into<String>) -> Self {
        let id = id.into();
        let mut error = Self::new(codes::OBJECT_TYPE_MISMATCH, format!("{} `{id}` does not accept this operation", kind.as_str()));
        error.object = Some((kind, id));
        error
    }

    /// `object/unsupported`: the object type is not editable yet.
    pub fn unsupported(feature: impl Into<String>) -> Self {
        let feature = feature.into();
        let mut error = Self::new(codes::OBJECT_UNSUPPORTED, format!("`{feature}` is not editable yet"));
        error.feature = Some(feature);
        error
    }

    /// `asset/missing`: the asset has no bytes or record.
    pub fn asset_missing(asset: impl Into<String>) -> Self {
        let asset = asset.into();
        let mut error = Self::new(codes::ASSET_MISSING, format!("asset `{asset}` does not exist")).with_recoverable(true);
        error.asset = Some(asset);
        error
    }

    /// `asset/in-use`: an element still places the asset.
    pub fn asset_in_use(asset: impl Into<String>) -> Self {
        let asset = asset.into();
        let mut error = Self::new(codes::ASSET_IN_USE, format!("asset `{asset}` is still placed by an element"));
        error.asset = Some(asset);
        error
    }

    /// `format/unsupported`: this build does not support the format.
    pub fn format_unsupported(format: impl Into<String>) -> Self {
        let format = format.into();
        let mut error = Self::new(codes::FORMAT_UNSUPPORTED, format!("format `{format}` is not supported by this build"));
        error.format = Some(format);
        error
    }

    /// Locates the failure at a committed revision.
    pub fn with_revision(mut self, revision: u64) -> Self {
        self.revision = Some(revision);
        self
    }

    /// Records the object a location-bearing code addresses.
    ///
    /// Use this when the message needs more context than the standard
    /// [`DeckError::unknown`] wording while the payload stays frozen.
    pub fn with_object(mut self, kind: params::ObjectKind, id: impl Into<String>) -> Self {
        self.object = Some((kind, id.into()));
        self
    }

    /// Records the format a `format/*` code reports.
    pub fn with_format(mut self, format: impl Into<String>) -> Self {
        self.format = Some(format.into());
        self
    }

    /// Records the revision the caller expected beside the committed one.
    pub fn with_expected(mut self, expected: u64) -> Self {
        self.expected = Some(expected);
        self
    }

    /// Locates the failure at a slide.
    pub fn with_slide(mut self, slide_id: SlideId) -> Self {
        self.slide_id = Some(slide_id);
        self
    }

    /// Locates the failure at an element.
    pub fn with_element(mut self, element_id: ElementId) -> Self {
        self.element_id = Some(element_id);
        self
    }

    /// Marks whether retrying with different input or state can succeed.
    pub fn with_recoverable(mut self, recoverable: bool) -> Self {
        self.recoverable = recoverable;
        self
    }

    /// Stable, comparable code.
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// Human-readable message; never carries binary payloads.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Revision the failure belongs to, when known.
    pub fn revision(&self) -> Option<u64> {
        self.revision
    }

    /// Revision the caller expected, when the script declared one.
    pub fn expected(&self) -> Option<u64> {
        self.expected
    }

    /// Slide the failure belongs to, when known.
    pub fn slide_id(&self) -> Option<&SlideId> {
        self.slide_id.as_ref()
    }

    /// Element the failure belongs to, when known.
    pub fn element_id(&self) -> Option<&ElementId> {
        self.element_id.as_ref()
    }

    /// Whether the caller can retry successfully.
    pub fn recoverable(&self) -> bool {
        self.recoverable
    }

    /// Typed payload for this code, derived from the locators the failure carries.
    ///
    /// `None` means the code has no frozen shape, or the failure was raised
    /// without the data its shape requires; bindings must not invent one.
    pub fn params(&self) -> Option<params::ErrorParams> {
        use params::{ErrorParams, ObjectKind};
        match self.code {
            codes::DECK_NOT_FOUND => Some(ErrorParams::DeckNotFound),
            codes::DECK_REVISION_CONFLICT => {
                Some(ErrorParams::RevisionConflict { actual: self.revision?, expected: self.expected })
            }
            codes::OBJECT_NOT_FOUND | codes::OBJECT_TYPE_MISMATCH => {
                let (kind, id) = match &self.object {
                    Some((kind, id)) => (*kind, id.clone()),
                    None => match (&self.element_id, &self.slide_id) {
                        (Some(element_id), _) => (ObjectKind::Element, element_id.as_str().to_owned()),
                        (None, Some(slide_id)) => (ObjectKind::Slide, slide_id.as_str().to_owned()),
                        (None, None) => return None,
                    },
                };
                Some(if self.code == codes::OBJECT_NOT_FOUND {
                    ErrorParams::NotFound { kind, id }
                }
                else {
                    ErrorParams::TypeMismatch { kind, id }
                })
            }
            codes::OBJECT_UNSUPPORTED => Some(ErrorParams::Unsupported { feature: self.feature.clone()? }),
            codes::ASSET_MISSING => Some(ErrorParams::AssetMissing { asset_id: self.asset.clone()? }),
            codes::ASSET_IN_USE => Some(ErrorParams::AssetInUse { asset_id: self.asset.clone()? }),
            codes::ARGUMENT_INVALID => Some(ErrorParams::ArgumentInvalid { reason: self.message.clone() }),
            codes::FORMAT_INVALID_PACKAGE => Some(ErrorParams::InvalidPackage { reason: self.message.clone() }),
            codes::FORMAT_UNSUPPORTED => Some(ErrorParams::UnsupportedFormat { format: self.format.clone()? }),
            codes::PLAYBACK_INVALID_STATE => Some(ErrorParams::PlaybackInvalidState { reason: self.message.clone() }),
            _ => None,
        }
    }

    /// Typed payload, refusing a failure that cannot fill its own shape.
    pub fn frozen_params(&self) -> Result<params::ErrorParams, params::ParamError> {
        self.params().ok_or(params::ParamError::Unstructured(self.code))
    }
}


impl std::fmt::Display for DeckError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DeckError {}
