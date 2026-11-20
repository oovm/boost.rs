//! The decoded ONNX model.
//!
//! These types mirror `onnx.proto` closely enough that a reader familiar with
//! the schema can check them against it, while dropping the parts a consumer
//! never uses (documentation strings, sparse tensors, type-protocol variants
//! beyond tensors). Anything the reader does not model is skipped as an unknown
//! field rather than rejected.

use crate::dtype::{AttributeType, DataType};

/// A whole `ModelProto`.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub ir_version: i64,
    pub opset_imports: Vec<OperatorSetId>,
    pub producer_name: String,
    pub producer_version: String,
    pub domain: String,
    pub model_version: i64,
    pub graph: Graph,
    pub metadata: Vec<(String, String)>,
}

impl Model {
    /// The version imported for `domain`, or `None` when the model does not
    /// import it. The empty string is ONNX's standard operator domain.
    pub fn opset_version(&self, domain: &str) -> Option<i64> {
        self.opset_imports.iter().find(|import| import.domain == domain).map(|import| import.version)
    }

    /// The standard-domain opset version, which is what most operators resolve
    /// against.
    pub fn ai_onnx_version(&self) -> Option<i64> {
        self.opset_version("")
    }

    /// Look up an initializer by name.
    pub fn initializer(&self, name: &str) -> Option<&Tensor> {
        self.graph.initializers.iter().find(|tensor| tensor.name == name)
    }

    /// Names the graph declares as inputs.
    pub fn input_names(&self) -> impl Iterator<Item = &str> {
        self.graph.inputs.iter().map(ValueInfo::name)
    }

    /// Names the graph declares as outputs.
    pub fn output_names(&self) -> impl Iterator<Item = &str> {
        self.graph.outputs.iter().map(ValueInfo::name)
    }
}

/// `OperatorSetIdProto`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorSetId {
    pub domain: String,
    pub version: i64,
}

/// `GraphProto`.
#[derive(Debug, Clone, PartialEq)]
pub struct Graph {
    pub name: String,
    pub nodes: Vec<Node>,
    pub initializers: Vec<Tensor>,
    pub inputs: Vec<ValueInfo>,
    pub outputs: Vec<ValueInfo>,
    /// Intermediate value types, when the producer recorded them.
    pub value_info: Vec<ValueInfo>,
}

/// `NodeProto`.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub op_type: String,
    /// Empty for the standard `ai.onnx` domain.
    pub domain: String,
    pub name: String,
    /// An empty string means an omitted optional input, which ONNX encodes
    /// positionally.
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub attributes: Vec<Attribute>,
}

impl Node {
    /// Find an attribute by name.
    pub fn attribute(&self, name: &str) -> Option<&Attribute> {
        self.attributes.iter().find(|attribute| attribute.name == name)
    }

    /// Whether the node is in `domain`, treating the empty string as
    /// `ai.onnx`.
    pub fn in_domain(&self, domain: &str) -> bool {
        self.domain == domain || (domain == "ai.onnx" && self.domain.is_empty())
    }
}

/// `AttributeProto`.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub name: String,
    pub kind: AttributeKind,
}

impl Attribute {
    /// The declared type tag. Derived from `kind`, so it cannot drift from the
    /// payload actually decoded.
    pub fn attribute_type(&self) -> AttributeType {
        match &self.kind {
            AttributeKind::Float(_) => AttributeType::Float,
            AttributeKind::Int(_) => AttributeType::Int,
            AttributeKind::String(_) => AttributeType::String,
            AttributeKind::Tensor(_) => AttributeType::Tensor,
            AttributeKind::Graph(_) => AttributeType::Graph,
            AttributeKind::Floats(_) => AttributeType::Floats,
            AttributeKind::Ints(_) => AttributeType::Ints,
            AttributeKind::Strings(_) => AttributeType::Strings,
            AttributeKind::Tensors(_) => AttributeType::Tensors,
            AttributeKind::Graphs(_) => AttributeType::Graphs,
            AttributeKind::Type(_) => AttributeType::TypeProto,
            AttributeKind::Undefined => AttributeType::Undefined,
        }
    }

    /// The single-precision value, if this is a scalar float attribute.
    pub fn as_float(&self) -> Option<f32> {
        match &self.kind {
            AttributeKind::Float(v) => Some(*v),
            _ => None,
        }
    }

    /// The integer value, if this is a scalar int attribute.
    pub fn as_int(&self) -> Option<i64> {
        match &self.kind {
            AttributeKind::Int(v) => Some(*v),
            _ => None,
        }
    }

    /// The byte-string value, if this is a scalar string attribute.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match &self.kind {
            AttributeKind::String(v) => Some(v),
            _ => None,
        }
    }

    /// The string value, when it is valid UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        self.as_bytes().and_then(|bytes| std::str::from_utf8(bytes).ok())
    }

    /// The list value, if this is a repeated int attribute.
    pub fn as_ints(&self) -> Option<&[i64]> {
        match &self.kind {
            AttributeKind::Ints(v) => Some(v),
            _ => None,
        }
    }

    /// The list value, if this is a repeated float attribute.
    pub fn as_floats(&self) -> Option<&[f32]> {
        match &self.kind {
            AttributeKind::Floats(v) => Some(v),
            _ => None,
        }
    }
}

/// The payload an attribute carries.
#[derive(Debug, Clone, PartialEq)]
pub enum AttributeKind {
    Float(f32),
    Int(i64),
    String(Vec<u8>),
    Tensor(Box<Tensor>),
    Graph(Box<Graph>),
    Floats(Vec<f32>),
    Ints(Vec<i64>),
    Strings(Vec<Vec<u8>>),
    Tensors(Vec<Tensor>),
    Graphs(Vec<Graph>),
    /// Element type of a `TENSOR_TYPE`/`TENSOR_LIST_TYPE` style attribute.
    Type(TypeShape),
    Undefined,
}

/// `TensorProto`.
#[derive(Debug, Clone, PartialEq)]
pub struct Tensor {
    pub name: String,
    /// `None` for the undefined tag `0`, which older producers leave unset on
    /// empty initializers.
    pub data_type: Option<DataType>,
    pub data_type_tag: i32,
    pub dims: Vec<i64>,
    pub payload: Payload,
    pub external: Option<ExternalData>,
}

impl Tensor {
    /// Total element count the declared shape implies, or `None` if a
    /// dimension is negative or the product overflows.
    pub fn element_count(&self) -> Option<u64> {
        let mut total = 1u64;
        for &dim in &self.dims {
            let dim = u64::try_from(dim).ok()?;
            total = total.checked_mul(dim)?;
        }
        Some(total)
    }

    /// Shape as `usize`, rejecting negative dimensions.
    pub fn shape(&self) -> Option<Vec<usize>> {
        self.dims.iter().map(|&dim| usize::try_from(dim).ok()).collect()
    }

    /// Number of elements actually carried by the payload.
    pub fn payload_elements(&self) -> Option<u64> {
        match &self.payload {
            Payload::Empty => Some(0),
            Payload::Strings(v) => Some(v.len() as u64),
            Payload::External { .. } => None,
            Payload::Bytes(bytes) => {
                let dtype = self.data_type?;
                if dtype == DataType::String {
                    return None;
                }
                let bits = dtype.bits();
                if bits == 0 {
                    return None;
                }
                if bits >= 8 {
                    let width = bits / 8;
                    if bytes.len() % width != 0 {
                        return None;
                    }
                    Some(bytes.len() as u64 / width as u64)
                } else {
                    // Sub-byte types pack two elements per byte.
                    Some((bytes.len() as u64) * (8 / bits as u64))
                }
            }
        }
    }

    /// The payload as little-endian bytes.
    ///
    /// A producer may have written a typed list (`float_data`, `int64_data`, …)
    /// instead of `raw_data`; the decoder normalises both to the same bytes, so
    /// a consumer never has to branch on which form the file happened to use.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match &self.payload {
            Payload::Empty => Some(&[]),
            Payload::Bytes(bytes) => Some(bytes),
            Payload::Strings(_) | Payload::External { .. } => None,
        }
    }

    /// Elements as single-precision floats.
    ///
    /// Deliberately narrow: it only succeeds for `FLOAT`, so a half-precision
    /// or quantised tensor cannot be silently reinterpreted as `f32`.
    pub fn as_f32(&self) -> Option<Vec<f32>> {
        if self.data_type != Some(DataType::Float) {
            return None;
        }
        let bytes = self.as_bytes()?;
        let width = DataType::Float.bytes_per_element();
        if bytes.len() % width != 0 {
            return None;
        }
        bytes
            .chunks_exact(width)
            .map(|chunk| Some(f32::from_le_bytes(chunk.try_into().ok()?)))
            .collect()
    }

    /// Elements as 64-bit integers.
    pub fn as_i64(&self) -> Option<Vec<i64>> {
        if self.data_type != Some(DataType::Int64) {
            return None;
        }
        let bytes = self.as_bytes()?;
        let width = DataType::Int64.bytes_per_element();
        if bytes.len() % width != 0 {
            return None;
        }
        bytes
            .chunks_exact(width)
            .map(|chunk| Some(i64::from_le_bytes(chunk.try_into().ok()?)))
            .collect()
    }

    /// Elements as 32-bit integers.
    pub fn as_i32(&self) -> Option<Vec<i32>> {
        if self.data_type != Some(DataType::Int32) {
            return None;
        }
        let bytes = self.as_bytes()?;
        let width = DataType::Int32.bytes_per_element();
        if bytes.len() % width != 0 {
            return None;
        }
        bytes
            .chunks_exact(width)
            .map(|chunk| Some(i32::from_le_bytes(chunk.try_into().ok()?)))
            .collect()
    }

    /// Elements as one byte each, which is how `BOOL` and the 8-bit integer
    /// types are stored.
    pub fn as_u8(&self) -> Option<Vec<u8>> {
        let bytes = self.as_bytes()?.to_vec();
        matches!(
            self.data_type,
            Some(DataType::Bool | DataType::Uint8 | DataType::Int8 | DataType::Float8E4M3Fn | DataType::Float8E5M2)
        )
        .then_some(bytes)
    }

    /// The string elements, when the tensor holds `STRING`.
    pub fn as_strings(&self) -> Option<&[Vec<u8>]> {
        match &self.payload {
            Payload::Strings(v) => Some(v),
            _ => None,
        }
    }
}

/// Where a tensor's elements live.
#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    /// No elements at all.
    Empty,
    /// Little-endian element bytes, from `raw_data` or normalised from a typed
    /// list field.
    Bytes(Vec<u8>),
    /// `string_data`, which has no fixed element width.
    Strings(Vec<Vec<u8>>),
    /// Weight bytes live in a sidecar file the importer was not allowed to open.
    External { location: String },
}

/// `TensorProto.external_data` resolved into the fields consumers need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalData {
    pub location: String,
    pub offset: u64,
    pub length: u64,
    pub checksum: Option<String>,
}

/// `ValueInfoProto`.
#[derive(Debug, Clone, PartialEq)]
pub struct ValueInfo {
    pub name: String,
    pub type_: TypeShape,
}

impl ValueInfo {
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// The subset of `TypeProto` that matters for graph edges: a tensor's element
/// type and its declared shape.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TypeShape {
    /// `TensorProto.DataType` tag, or `None` when the producer left it unset.
    pub elem_type: Option<i32>,
    /// One entry per axis. `None` is a symbolic or unknown axis.
    pub dims: Vec<Option<Dimension>>,
    /// Set when the value is not a plain tensor (sequence, map, optional).
    pub non_tensor: Option<NonTensor>,
}

/// One axis of a declared shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dimension {
    Known(i64),
    Symbolic(String),
}

/// A value type that is not a dense tensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonTensor {
    Sequence,
    Map,
    Optional,
    SparseTensor,
}
