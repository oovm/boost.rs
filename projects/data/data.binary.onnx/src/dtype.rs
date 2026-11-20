//! ONNX element types and the protobuf field numbers this reader depends on.
//!
//! The field numbers are the wire contract of `onnx.proto`. They are pinned
//! here rather than scattered through the decoder so that a schema change in a
//! future ONNX revision is one reviewable diff, and so a reader of this file
//! can check it against the upstream `.proto` without reading any parsing code.

use core::fmt;

/// `TensorProto.DataType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Float,
    Uint8,
    Int8,
    Uint16,
    Int16,
    Uint32,
    Int32,
    Uint64,
    Int64,
    String,
    Bool,
    Float16,
    Bfloat16,
    Complex64,
    Complex128,
    QInt8,
    QUInt8,
    QInt32,
    QUInt32,
    QInt16,
    QUInt16,
    QInt4,
    QUInt4,
    Float8E4M3Fn,
    Float8E4M3FNUz,
    Float8E5M2,
    Float8E5M2FNUz,
    Uint4,
    Int4,
}

impl DataType {
    /// Decode the numeric tag, or `None` for an undefined or future type.
    pub fn from_tag(tag: i32) -> Option<Self> {
        Some(match tag {
            1 => DataType::Float,
            2 => DataType::Uint8,
            3 => DataType::Int8,
            4 => DataType::Uint16,
            5 => DataType::Int16,
            6 => DataType::Uint32,
            7 => DataType::Int32,
            8 => DataType::Uint64,
            9 => DataType::Int64,
            10 => DataType::String,
            11 => DataType::Bool,
            12 => DataType::Float16,
            13 => DataType::Bfloat16,
            14 => DataType::Complex64,
            15 => DataType::Complex128,
            16 => DataType::QInt8,
            17 => DataType::QUInt8,
            18 => DataType::QInt32,
            19 => DataType::QUInt32,
            20 => DataType::QInt16,
            21 => DataType::QUInt16,
            22 => DataType::QInt4,
            23 => DataType::QUInt4,
            24 => DataType::Float8E4M3Fn,
            25 => DataType::Float8E4M3FNUz,
            26 => DataType::Float8E5M2,
            27 => DataType::Float8E5M2FNUz,
            28 => DataType::Uint4,
            29 => DataType::Int4,
            _ => return None,
        })
    }

    /// The numeric tag written into `TensorProto.data_type`.
    pub const fn tag(self) -> i32 {
        match self {
            DataType::Float => 1,
            DataType::Uint8 => 2,
            DataType::Int8 => 3,
            DataType::Uint16 => 4,
            DataType::Int16 => 5,
            DataType::Uint32 => 6,
            DataType::Int32 => 7,
            DataType::Uint64 => 8,
            DataType::Int64 => 9,
            DataType::String => 10,
            DataType::Bool => 11,
            DataType::Float16 => 12,
            DataType::Bfloat16 => 13,
            DataType::Complex64 => 14,
            DataType::Complex128 => 15,
            DataType::QInt8 => 16,
            DataType::QUInt8 => 17,
            DataType::QInt32 => 18,
            DataType::QUInt32 => 19,
            DataType::QInt16 => 20,
            DataType::QUInt16 => 21,
            DataType::QInt4 => 22,
            DataType::QUInt4 => 23,
            DataType::Float8E4M3Fn => 24,
            DataType::Float8E4M3FNUz => 25,
            DataType::Float8E5M2 => 26,
            DataType::Float8E5M2FNUz => 27,
            DataType::Uint4 => 28,
            DataType::Int4 => 29,
        }
    }

    /// Bytes one element occupies in `raw_data`.
    ///
    /// Sub-byte quantised types report the number of *whole* elements that fit
    /// in one byte, so callers that need bit packing ask for `bits()` instead.
    pub const fn bytes_per_element(self) -> usize {
        match self {
            DataType::Float | DataType::Int32 | DataType::Uint32 | DataType::Complex64 | DataType::QInt32 | DataType::QUInt32 => 4,
            DataType::Float16 | DataType::Bfloat16 | DataType::Int16 | DataType::Uint16 | DataType::QInt16 | DataType::QUInt16 => 2,
            DataType::Int64 | DataType::Uint64 | DataType::Complex128 => 8,
            DataType::Float8E4M3Fn | DataType::Float8E4M3FNUz | DataType::Float8E5M2 | DataType::Float8E5M2FNUz => 1,
            DataType::Int8 | DataType::Uint8 | DataType::Bool | DataType::QInt8 | DataType::QUInt8 => 1,
            DataType::QInt4 | DataType::QUInt4 | DataType::Int4 | DataType::Uint4 => 1,
            // Strings have no fixed width; their payload is a list of byte runs.
            DataType::String => 0,
        }
    }

    /// Bits one element occupies. Four-bit types pack two elements per byte.
    pub const fn bits(self) -> usize {
        match self {
            DataType::QInt4 | DataType::QUInt4 | DataType::Int4 | DataType::Uint4 => 4,
            other => other.bytes_per_element() * 8,
        }
    }

    /// Whether the type is one this reader can hand back as typed elements
    /// rather than as opaque bytes.
    pub const fn materialisable(self) -> bool {
        matches!(
            self,
            DataType::Float | DataType::Int32 | DataType::Int64 | DataType::Uint64 | DataType::Bool | DataType::String
        )
    }

    /// The `TensorProto` field that carries a typed (non-raw) list for this
    /// element type, if the schema defines one.
    pub const fn typed_field(self) -> Option<u32> {
        match self {
            DataType::Float => Some(field::TENSOR_FLOAT_DATA),
            DataType::Int32 | DataType::Bool => Some(field::TENSOR_INT32_DATA),
            DataType::Int64 => Some(field::TENSOR_INT64_DATA),
            DataType::Uint64 => Some(field::TENSOR_UINT64_DATA),
            DataType::String => Some(field::TENSOR_STRING_DATA),
            _ => None,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DataType::Float => "FLOAT",
            DataType::Uint8 => "UINT8",
            DataType::Int8 => "INT8",
            DataType::Uint16 => "UINT16",
            DataType::Int16 => "INT16",
            DataType::Uint32 => "UINT32",
            DataType::Int32 => "INT32",
            DataType::Uint64 => "UINT64",
            DataType::Int64 => "INT64",
            DataType::String => "STRING",
            DataType::Bool => "BOOL",
            DataType::Float16 => "FLOAT16",
            DataType::Bfloat16 => "BFLOAT16",
            DataType::Complex64 => "COMPLEX64",
            DataType::Complex128 => "COMPLEX128",
            DataType::QInt8 => "QINT8",
            DataType::QUInt8 => "QUINT8",
            DataType::QInt32 => "QINT32",
            DataType::QUInt32 => "QUINT32",
            DataType::QInt16 => "QINT16",
            DataType::QUInt16 => "QUINT16",
            DataType::QInt4 => "QINT4",
            DataType::QUInt4 => "QUINT4",
            DataType::Float8E4M3Fn => "FLOAT8E4M3FN",
            DataType::Float8E4M3FNUz => "FLOAT8E4M3FNUZ",
            DataType::Float8E5M2 => "FLOAT8E5M2",
            DataType::Float8E5M2FNUz => "FLOAT8E5M2FNUZ",
            DataType::Uint4 => "UINT4",
            DataType::Int4 => "INT4",
        })
    }
}

/// `AttributeProto.AttributeType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttributeType {
    Undefined,
    Float,
    Int,
    String,
    Tensor,
    Graph,
    SparseTensor,
    TypeProto,
    Floats,
    Ints,
    Strings,
    Tensors,
    Graphs,
    SparseTensors,
    TypeProtos,
}

impl AttributeType {
    pub fn from_tag(tag: i32) -> Self {
        match tag {
            1 => AttributeType::Float,
            2 => AttributeType::Int,
            3 => AttributeType::String,
            4 => AttributeType::Tensor,
            5 => AttributeType::Graph,
            6 => AttributeType::SparseTensor,
            7 => AttributeType::TypeProto,
            8 => AttributeType::Floats,
            9 => AttributeType::Ints,
            10 => AttributeType::Strings,
            11 => AttributeType::Tensors,
            12 => AttributeType::Graphs,
            13 => AttributeType::SparseTensors,
            14 => AttributeType::TypeProtos,
            _ => AttributeType::Undefined,
        }
    }
}

/// Protobuf field numbers, grouped by message.
pub mod field {
    /// `ModelProto`
    pub const MODEL_IR_VERSION: u32 = 1;
    pub const MODEL_OPSET_IMPORT: u32 = 2;
    pub const MODEL_PRODUCER_NAME: u32 = 3;
    pub const MODEL_PRODUCER_VERSION: u32 = 4;
    pub const MODEL_DOMAIN: u32 = 5;
    pub const MODEL_VERSION: u32 = 6;
    pub const MODEL_GRAPH: u32 = 7;
    pub const MODEL_DOC_STRING: u32 = 8;
    pub const MODEL_METADATA_PROPS: u32 = 13;

    /// `OperatorSetIdProto`
    pub const OPSET_DOMAIN: u32 = 1;
    pub const OPSET_VERSION: u32 = 2;

    /// `GraphProto`
    pub const GRAPH_NODE: u32 = 1;
    pub const GRAPH_NAME: u32 = 2;
    pub const GRAPH_INITIALIZER: u32 = 3;
    pub const GRAPH_DOC_STRING: u32 = 4;
    pub const GRAPH_INPUT: u32 = 8;
    pub const GRAPH_OUTPUT: u32 = 9;
    pub const GRAPH_VALUE_INFO: u32 = 10;
    pub const GRAPH_SPARSE_INITIALIZER: u32 = 11;

    /// `NodeProto`
    pub const NODE_INPUT: u32 = 1;
    pub const NODE_OUTPUT: u32 = 2;
    pub const NODE_NAME: u32 = 3;
    pub const NODE_OP_TYPE: u32 = 4;
    pub const NODE_ATTRIBUTE: u32 = 5;
    pub const NODE_DOC_STRING: u32 = 7;
    pub const NODE_DOMAIN: u32 = 8;

    /// `AttributeProto`
    pub const ATTR_NAME: u32 = 1;
    pub const ATTR_TYPE: u32 = 2;
    pub const ATTR_FLOAT: u32 = 3;
    pub const ATTR_INT: u32 = 4;
    pub const ATTR_STRING: u32 = 5;
    pub const ATTR_TENSOR: u32 = 6;
    pub const ATTR_GRAPH: u32 = 7;
    pub const ATTR_FLOATS: u32 = 8;
    pub const ATTR_INTS: u32 = 9;
    pub const ATTR_STRINGS: u32 = 10;
    pub const ATTR_TENSORS: u32 = 11;
    pub const ATTR_GRAPHS: u32 = 12;
    pub const ATTR_SPARSE_TENSOR: u32 = 14;
    pub const ATTR_TYPE_PROTO: u32 = 15;
    pub const ATTR_TYPE_PROTOS: u32 = 16;

    /// `TensorProto`
    pub const TENSOR_DIMS: u32 = 1;
    pub const TENSOR_DATA_TYPE: u32 = 2;
    pub const TENSOR_FLOAT_DATA: u32 = 4;
    pub const TENSOR_INT32_DATA: u32 = 5;
    pub const TENSOR_STRING_DATA: u32 = 6;
    pub const TENSOR_INT64_DATA: u32 = 7;
    pub const TENSOR_NAME: u32 = 8;
    pub const TENSOR_RAW_DATA: u32 = 9;
    pub const TENSOR_EXTERNAL_DATA: u32 = 10;
    pub const TENSOR_DATA_LOCATION: u32 = 11;
    pub const TENSOR_DOUBLE_DATA: u32 = 12;
    pub const TENSOR_UINT64_DATA: u32 = 13;

    /// `StringStringEntryProto`
    pub const ENTRY_KEY: u32 = 1;
    pub const ENTRY_VALUE: u32 = 2;

    /// `ValueInfoProto`
    pub const VALUE_NAME: u32 = 1;
    pub const VALUE_TYPE: u32 = 2;
    pub const VALUE_DOC_STRING: u32 = 3;

    /// `TypeProto`
    pub const TYPE_TENSOR: u32 = 1;
    pub const TYPE_SEQUENCE: u32 = 2;
    pub const TYPE_MAP: u32 = 3;
    pub const TYPE_SPARSE_TENSOR: u32 = 5;

    /// `TypeProto.Tensor`
    pub const TENSOR_TYPE_ELEM_TYPE: u32 = 1;
    pub const TENSOR_TYPE_SHAPE: u32 = 2;

    /// `TensorShapeProto`
    pub const SHAPE_DIM: u32 = 1;

    /// `TensorShapeProto.Dimension`
    pub const DIM_VALUE: u32 = 1;
    pub const DIM_PARAM: u32 = 2;
}
