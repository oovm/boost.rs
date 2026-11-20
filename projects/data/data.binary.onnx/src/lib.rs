//! Hand-written **ONNX** `ModelProto` reader.
//!
//! ONNX stores its models with the protobuf encoding, so this crate is a thin
//! schema layer over [`data_binary_protobuf`]. It is written by hand on purpose:
//! generated stubs would model the `.proto` faithfully while saying nothing of
//! the things a consumer actually has to know before it trusts a file — which
//! opset a node resolves against, whether an initializer's declared shape
//! matches the bytes it carries, whether weights were left in a sidecar the
//! importer was never told about.
//!
//! # What it guarantees
//!
//! * **Version gates.** `ir_version` outside the range this decoder interprets
//!   is refused rather than half-read, and a model that imports no opset is
//!   refused because no operator can then be resolved.
//! * **One storage form per tensor.** `raw_data`, `float_data`/`int32_data`/…,
//!   `string_data` and `external_data` are mutually exclusive; a file mixing
//!   them is rejected instead of silently preferring one.
//! * **Shape/bytes agreement.** Every named initializer must carry exactly the
//!   element count its `dims` claim, so a truncated weight file fails at import
//!   rather than at first inference.
//! * **Packed and non-packed repeats.** Producers emit both; either decodes to
//!   the same list.
//! * **No ambient file access.** A tensor whose weights live in a sidecar is an
//!   error by default. Reading `location` would let a model file name arbitrary
//!   paths, so it takes an explicit [`Config::allow_external_data`].
//!
//! # What it deliberately does not do
//!
//! No shape inference, no operator semantics, no execution plan, no graph
//! rewriting, and no mapping onto a particular DL IR. Those need the consumer's
//! own type; this crate stops at a faithful, validated description of the file.
//!
//! ```
//! use data_binary_onnx::decode;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // A minimal but complete model: one Relu node, standard opset 17.
//! let bytes = data_binary_onnx::testkit::minimal_model(17);
//! let model = decode(&bytes)?;
//! assert_eq!(model.ai_onnx_version(), Some(17));
//! assert_eq!(model.graph.nodes.len(), 1);
//! assert_eq!(model.graph.nodes[0].op_type, "Relu");
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

mod decode;
mod dtype;
mod error;
mod model;

pub use decode::{
    Config, DEFAULT_MAX_SUBGRAPH_DEPTH, MAX_SUPPORTED_IR_VERSION, MIN_SUPPORTED_IR_VERSION, decode, decode_with,
};
pub use dtype::{AttributeType, DataType};
pub use error::OnnxError;
pub use model::{
    Attribute, AttributeKind, Dimension, ExternalData, Graph, Model, Node, NonTensor, OperatorSetId, Payload, Tensor,
    TypeShape, ValueInfo,
};

/// Read a model straight from disk.
pub fn decode_file(path: &std::path::Path) -> Result<Model, OnnxError> {
    let bytes = std::fs::read(path).map_err(|error| OnnxError::Wire(format!("{}: {error}", path.display())))?;
    decode(&bytes)
}

/// Encoders that produce valid ONNX bytes, used by this crate's own tests and
/// available to downstream tests instead of checked-in `.onnx` binaries.
pub mod testkit {
    use data_binary_protobuf::Writer;

    /// A model with one `Relu` node, standard-domain opset `version`.
    pub fn minimal_model(version: i64) -> Vec<u8> {
        let mut node = Writer::new();
        node.write_string(1, "x").unwrap();
        node.write_string(2, "y").unwrap();
        node.write_string(4, "Relu").unwrap();

        let mut opset = Writer::new();
        opset.write_string(1, "").unwrap();
        opset.write_i64(2, version).unwrap();

        let mut graph = Writer::new();
        graph.write_string(2, "main").unwrap();
        graph.write_message(1, node.as_slice()).unwrap();
        graph.write_message(8, value_info("x", 1, &[3, 4]).as_slice()).unwrap();
        graph.write_message(9, value_info("y", 1, &[3, 4]).as_slice()).unwrap();

        let mut model = Writer::new();
        model.write_i64(1, 9).unwrap();
        model.write_message(2, opset.as_slice()).unwrap();
        model.write_string(3, "boost-testkit").unwrap();
        model.write_message(7, graph.as_slice()).unwrap();
        model.into_vec()
    }

    /// A `FLOAT` initializer named `name` holding `values`, shaped `[len]`.
    pub fn initializer(name: &str, values: &[f32]) -> Vec<u8> {
        let mut tensor = Writer::new();
        tensor.write_i64(1, values.len() as i64).unwrap();
        tensor.write_i32(2, 1).unwrap();
        tensor.write_string(8, name).unwrap();
        let mut raw = Vec::new();
        for value in values {
            raw.extend_from_slice(&value.to_le_bytes());
        }
        tensor.write_bytes(9, &raw).unwrap();
        tensor.into_vec()
    }

    /// A `ValueInfoProto` for a dense tensor of `elem_type` and fixed `dims`.
    pub fn value_info(name: &str, elem_type: i32, dims: &[i64]) -> Vec<u8> {
        let mut shape = Writer::new();
        for &dim in dims {
            let mut axis = Writer::new();
            axis.write_i64(1, dim).unwrap();
            shape.write_message(1, axis.as_slice()).unwrap();
        }
        let mut tensor_type = Writer::new();
        tensor_type.write_i32(1, elem_type).unwrap();
        tensor_type.write_message(2, shape.as_slice()).unwrap();
        let mut type_proto = Writer::new();
        type_proto.write_message(1, tensor_type.as_slice()).unwrap();

        let mut info = Writer::new();
        info.write_string(1, name).unwrap();
        info.write_message(2, type_proto.as_slice()).unwrap();
        info.into_vec()
    }
}
