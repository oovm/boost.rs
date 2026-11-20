//! Errors raised while importing an ONNX model.

/// Reasons a `ModelProto` is rejected.
///
/// The variants separate *transport* problems (the bytes are not protobuf, or
/// run out mid-message) from *schema* problems (the file is protobuf but not an
/// ONNX model this reader claims to understand). A caller deciding whether to
/// fall back to another importer only needs to look at which family fired.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OnnxError {
    #[error("wire decoding failed: {0}")]
    Wire(String),

    #[error("IR version {0} is not supported (this reader covers 3..={1})")]
    UnsupportedIrVersion(i64, i64),

    #[error("model declares no opset_import")]
    MissingOpset,

    #[error("model has no graph")]
    MissingGraph,

    #[error("opset domain {domain:?} requires version {required}, model imports {found}")]
    OpsetTooOld { domain: String, required: i64, found: i64 },

    #[error("tensor {name:?} declares {declared} elements but carries {actual}")]
    ElementCount { name: String, declared: u64, actual: u64 },

    #[error("tensor {name:?} has data_type {data_type}, which is not a defined ONNX element type")]
    UnknownDataType { name: String, data_type: i32 },

    #[error("tensor {name:?} carries both raw_data and typed data fields")]
    ConflictingPayload { name: String },

    #[error("tensor {name:?} stores its payload outside the file ({location}), which needs an explicit resolver")]
    ExternalData { name: String, location: String },

    #[error("node {name:?} has no op_type")]
    MissingOpType { name: String },

    #[error("node {name:?} repeats attribute {attribute:?}")]
    DuplicateAttribute { name: String, attribute: String },

    #[error("graph {name:?} repeats initializer {initializer:?}")]
    DuplicateInitializer { name: String, initializer: String },

    #[error("attribute {0:?} has an undefined type")]
    UndefinedAttribute(String),

    #[error("dimension of tensor {0:?} is negative")]
    NegativeDim(String),

    #[error("subgraph nesting exceeds the limit of {0} levels")]
    TooDeepSubgraphs(usize),
}

impl From<data_binary_protobuf::ProtoError> for OnnxError {
    fn from(error: data_binary_protobuf::ProtoError) -> Self {
        OnnxError::Wire(error.to_string())
    }
}
