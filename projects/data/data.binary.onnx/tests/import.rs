//! ONNX import smoke tests. Every model is assembled in code through the wire
//! substrate — no checked-in `.onnx` binaries.

use data_binary_onnx::{AttributeKind, Config, DataType, OnnxError, Payload, decode, decode_with, testkit};
use data_binary_protobuf::Writer;

/// Wrap a graph in a model with a standard-domain opset.
fn model_with(graph: &[u8], ir_version: i64, opset: i64) -> Vec<u8> {
    let mut opset_msg = Writer::new();
    opset_msg.write_string(1, "").unwrap();
    opset_msg.write_i64(2, opset).unwrap();

    let mut model = Writer::new();
    model.write_i64(1, ir_version).unwrap();
    model.write_message(2, opset_msg.as_slice()).unwrap();
    model.write_message(7, graph).unwrap();
    model.into_vec()
}

fn graph_with(fields: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut graph = Writer::new();
    graph.write_string(2, "g").unwrap();
    for (number, body) in fields {
        graph.write_message(*number, body).unwrap();
    }
    graph.into_vec()
}

fn node(op_type: &str, inputs: &[&str], outputs: &[&str], attrs: &[Vec<u8>]) -> Vec<u8> {
    let mut node = Writer::new();
    for input in inputs {
        node.write_string(1, input).unwrap();
    }
    for output in outputs {
        node.write_string(2, output).unwrap();
    }
    node.write_string(4, op_type).unwrap();
    for attr in attrs {
        node.write_message(5, attr).unwrap();
    }
    node.into_vec()
}

// -------------------------------------------------------------- happy path ---

#[test]
fn minimal_model_decodes() {
    let model = decode(&testkit::minimal_model(17)).unwrap();
    assert_eq!(model.ir_version, 9);
    assert_eq!(model.producer_name, "boost-testkit");
    assert_eq!(model.ai_onnx_version(), Some(17));
    assert_eq!(model.graph.name, "main");
    assert_eq!(model.graph.nodes.len(), 1);
    assert_eq!(model.graph.nodes[0].op_type, "Relu");
    assert_eq!(model.graph.nodes[0].inputs, vec!["x".to_string()]);
    assert_eq!(model.graph.nodes[0].outputs, vec!["y".to_string()]);
    assert!(model.graph.nodes[0].in_domain("ai.onnx"));
    assert_eq!(model.input_names().collect::<Vec<_>>(), vec!["x"]);
    assert_eq!(model.output_names().collect::<Vec<_>>(), vec!["y"]);
}

#[test]
fn declared_shapes_are_carried_through() {
    let model = decode(&testkit::minimal_model(13)).unwrap();
    let x = model.graph.inputs.iter().find(|info| info.name == "x").unwrap();
    assert_eq!(x.type_.elem_type, Some(DataType::Float.tag()));
    assert_eq!(
        x.type_.dims,
        vec![
            Some(data_binary_onnx::Dimension::Known(3)),
            Some(data_binary_onnx::Dimension::Known(4))
        ]
    );
}

#[test]
fn symbolic_axis_is_kept_distinct_from_a_known_one() {
    let mut axis = Writer::new();
    axis.write_string(2, "batch").unwrap();
    let mut shape = Writer::new();
    shape.write_message(1, axis.as_slice()).unwrap();
    let mut tensor_type = Writer::new();
    tensor_type.write_i32(1, DataType::Float.tag()).unwrap();
    tensor_type.write_message(2, shape.as_slice()).unwrap();
    let mut type_proto = Writer::new();
    type_proto.write_message(1, tensor_type.as_slice()).unwrap();
    let mut info = Writer::new();
    info.write_string(1, "x").unwrap();
    info.write_message(2, type_proto.as_slice()).unwrap();

    let mut graph = Writer::new();
    graph.write_message(8, info.as_slice()).unwrap();
    let model = decode(&model_with(graph.as_slice(), 9, 17)).unwrap();
    assert_eq!(model.graph.inputs[0].type_.dims, vec![Some(data_binary_onnx::Dimension::Symbolic("batch".into()))]);
}

#[test]
fn initializer_raw_data_becomes_f32_elements() {
    let graph = graph_with(&[(3, testkit::initializer("W", &[1.0, -2.5, 0.25, 3.75]))]);
    let model = decode(&model_with(&graph, 9, 17)).unwrap();
    let w = model.initializer("W").unwrap();
    assert_eq!(w.data_type, Some(DataType::Float));
    assert_eq!(w.dims, vec![4]);
    assert_eq!(w.element_count(), Some(4));
    assert_eq!(w.as_f32().unwrap(), vec![1.0, -2.5, 0.25, 3.75]);
}

/// A producer that used `float_data` instead of `raw_data` must land in the
/// same representation, so consumers never branch on storage form.
#[test]
fn typed_list_and_raw_data_normalise_identically() {
    let values = [1.0f32, -2.5, 0.25];

    let mut raw = Writer::new();
    raw.write_i64(1, 3).unwrap();
    raw.write_i32(2, DataType::Float.tag()).unwrap();
    raw.write_string(8, "T").unwrap();
    let mut bytes = Vec::new();
    for v in values {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    raw.write_bytes(9, &bytes).unwrap();

    let mut typed = Writer::new();
    typed.write_i64(1, 3).unwrap();
    typed.write_i32(2, DataType::Float.tag()).unwrap();
    typed.write_string(8, "T").unwrap();
    typed.write_packed(4, &values).unwrap();

    let a = decode(&model_with(&graph_with(&[(3, raw.into_vec())]), 9, 17)).unwrap();
    let b = decode(&model_with(&graph_with(&[(3, typed.into_vec())]), 9, 17)).unwrap();
    assert_eq!(a.initializer("T"), b.initializer("T"));
    assert_eq!(b.initializer("T").unwrap().as_f32().unwrap(), values);
}

#[test]
fn int64_initializer_from_int64_data() {
    let mut tensor = Writer::new();
    tensor.write_i64(1, 2).unwrap();
    tensor.write_i32(2, DataType::Int64.tag()).unwrap();
    tensor.write_string(8, "shape").unwrap();
    tensor.write_packed(7, &[1i64, 3]).unwrap();
    let model = decode(&model_with(&graph_with(&[(3, tensor.into_vec())]), 9, 17)).unwrap();
    assert_eq!(model.initializer("shape").unwrap().as_i64().unwrap(), vec![1, 3]);
}

/// Half-precision weights packed into `int32_data` must be narrowed to the
/// declared two bytes per element, not kept at four.
#[test]
fn fp16_packed_into_int32_data_is_narrowed_to_two_bytes() {
    let mut tensor = Writer::new();
    tensor.write_i64(1, 2).unwrap();
    tensor.write_i32(2, DataType::Float16.tag()).unwrap();
    tensor.write_string(8, "h").unwrap();
    tensor.write_packed(5, &[0x3c00u32, 0x4000]).unwrap();
    let model = decode(&model_with(&graph_with(&[(3, tensor.into_vec())]), 9, 17)).unwrap();
    let h = model.initializer("h").unwrap();
    assert_eq!(h.as_bytes().unwrap(), &[0x00, 0x3c, 0x00, 0x40]);
    assert_eq!(h.payload_elements(), Some(2));
    // A half tensor is deliberately not readable as f32.
    assert!(h.as_f32().is_none());
}

#[test]
fn bool_elements_are_one_byte_each() {
    let mut tensor = Writer::new();
    tensor.write_i64(1, 3).unwrap();
    tensor.write_i32(2, DataType::Bool.tag()).unwrap();
    tensor.write_string(8, "m").unwrap();
    tensor.write_packed(5, &[1i32, 0, 1]).unwrap();
    let model = decode(&model_with(&graph_with(&[(3, tensor.into_vec())]), 9, 17)).unwrap();
    assert_eq!(model.initializer("m").unwrap().as_u8().unwrap(), vec![1, 0, 1]);
}

#[test]
fn string_data_is_kept_as_byte_runs() {
    let mut tensor = Writer::new();
    tensor.write_i64(1, 2).unwrap();
    tensor.write_i32(2, DataType::String.tag()).unwrap();
    tensor.write_string(8, "labels").unwrap();
    tensor.write_bytes(6, b"cat").unwrap();
    tensor.write_bytes(6, b"dog").unwrap();
    let model = decode(&model_with(&graph_with(&[(3, tensor.into_vec())]), 9, 17)).unwrap();
    let t = model.initializer("labels").unwrap();
    assert_eq!(t.as_strings().unwrap().len(), 2);
    assert_eq!(t.as_strings().unwrap()[1], b"dog");
}

#[test]
fn scalar_tensor_with_no_dims_has_one_element() {
    let mut tensor = Writer::new();
    tensor.write_i32(2, DataType::Float.tag()).unwrap();
    tensor.write_string(8, "lr").unwrap();
    tensor.write_bytes(9, &1.5f32.to_le_bytes()).unwrap();
    let model = decode(&model_with(&graph_with(&[(3, tensor.into_vec())]), 9, 17)).unwrap();
    let lr = model.initializer("lr").unwrap();
    assert_eq!(lr.dims, Vec::<i64>::new());
    assert_eq!(lr.element_count(), Some(1));
    assert_eq!(lr.as_f32().unwrap(), vec![1.5]);
}

#[test]
fn attributes_decode_by_declared_type() {
    let mut axis = Writer::new();
    axis.write_string(1, "axis").unwrap();
    axis.write_i32(2, 2).unwrap();
    axis.write_i64(4, 1).unwrap();

    let mut perm = Writer::new();
    perm.write_string(1, "perm").unwrap();
    perm.write_i32(2, 9).unwrap();
    perm.write_packed(9, &[0i64, 2, 3, 1]).unwrap();

    let mut name = Writer::new();
    name.write_string(1, "mode").unwrap();
    name.write_i32(2, 3).unwrap();
    name.write_bytes(5, b"linear").unwrap();

    let graph = graph_with(&[(1, node("Transpose", &["x"], &["y"], &[axis.into_vec(), perm.into_vec(), name.into_vec()]))]);
    let model = decode(&model_with(&graph, 9, 17)).unwrap();
    let t = &model.graph.nodes[0];
    assert_eq!(t.attribute("axis").unwrap().as_int(), Some(1));
    assert_eq!(t.attribute("perm").unwrap().as_ints().unwrap(), vec![0, 2, 3, 1]);
    assert_eq!(t.attribute("mode").unwrap().as_str(), Some("linear"));
    assert_eq!(t.attribute("perm").unwrap().attribute_type(), data_binary_onnx::AttributeType::Ints);
    assert!(t.attribute("absent").is_none());
}

#[test]
fn legacy_attribute_without_type_is_inferred() {
    let mut attr = Writer::new();
    attr.write_string(1, "axes").unwrap();
    attr.write_packed(9, &[0i64, 1]).unwrap();
    let graph = graph_with(&[(1, node("Squeeze", &["x"], &["y"], &[attr.into_vec()]))]);
    let model = decode(&model_with(&graph, 9, 17)).unwrap();
    assert_eq!(model.graph.nodes[0].attribute("axes").unwrap().as_ints().unwrap(), vec![0, 1]);
}

#[test]
fn tensor_attribute_is_decoded() {
    let mut attr = Writer::new();
    attr.write_string(1, "value").unwrap();
    attr.write_i32(2, 4).unwrap();
    attr.write_message(6, &testkit::initializer("const", &[2.0, 3.0])).unwrap();
    let graph = graph_with(&[(1, node("Add", &["x"], &["y"], &[attr.into_vec()]))]);
    let model = decode(&model_with(&graph, 9, 17)).unwrap();
    match &model.graph.nodes[0].attribute("value").unwrap().kind {
        AttributeKind::Tensor(t) => {
            assert_eq!(t.name, "const");
            assert_eq!(t.as_f32().unwrap(), vec![2.0, 3.0]);
        }
        other => panic!("expected tensor attribute, got {other:?}"),
    }
}

#[test]
fn subgraph_attribute_is_decoded_with_depth_counted() {
    let mut body = Writer::new();
    body.write_string(2, "then").unwrap();
    body.write_message(1, &node("Neg", &["i"], &["o"], &[])).unwrap();

    let mut attr = Writer::new();
    attr.write_string(1, "then_branch").unwrap();
    attr.write_i32(2, 5).unwrap();
    attr.write_message(7, body.as_slice()).unwrap();

    let graph = graph_with(&[(1, node("If", &["c"], &["y"], &[attr.into_vec()]))]);
    let model = decode(&model_with(&graph, 9, 17)).unwrap();
    match &model.graph.nodes[0].attribute("then_branch").unwrap().kind {
        AttributeKind::Graph(inner) => {
            assert_eq!(inner.name, "then");
            assert_eq!(inner.nodes[0].op_type, "Neg");
        }
        other => panic!("expected graph attribute, got {other:?}"),
    }
}

/// Packed and non-packed repeats of the same field must decode alike; both
/// appear in files produced by different toolchain generations.
#[test]
fn packed_and_nonpacked_repeats_agree() {
    let mut packed = Writer::new();
    packed.write_packed(1, &[2i64, 3]).unwrap();
    packed.write_i32(2, DataType::Float.tag()).unwrap();
    packed.write_string(8, "P").unwrap();
    packed.write_bytes(9, &vec![0u8; 24]).unwrap();

    let mut unpacked = Writer::new();
    unpacked.write_i64(1, 2).unwrap();
    unpacked.write_i64(1, 3).unwrap();
    unpacked.write_i32(2, DataType::Float.tag()).unwrap();
    unpacked.write_string(8, "P").unwrap();
    unpacked.write_bytes(9, &vec![0u8; 24]).unwrap();

    let a = decode(&model_with(&graph_with(&[(3, packed.into_vec())]), 9, 17)).unwrap();
    let b = decode(&model_with(&graph_with(&[(3, unpacked.into_vec())]), 9, 17)).unwrap();
    assert_eq!(a.initializer("P"), b.initializer("P"));
    assert_eq!(a.initializer("P").unwrap().dims, vec![2, 3]);
}

#[test]
fn unknown_fields_are_skipped_so_newer_files_still_load() {
    let mut graph = Writer::new();
    graph.write_string(2, "g").unwrap();
    graph.write_message(1, &node("Relu", &["x"], &["y"], &[])).unwrap();
    // A field this decoder has never heard of.
    graph.write_string(99, "from a future revision").unwrap();
    let model = decode(&model_with(&graph.into_vec(), 9, 18)).unwrap();
    assert_eq!(model.graph.nodes[0].op_type, "Relu");
}

#[test]
fn metadata_entries_are_kept() {
    let mut entry = Writer::new();
    entry.write_string(1, "author").unwrap();
    entry.write_string(2, "someone").unwrap();
    let mut model = Writer::new();
    model.write_i64(1, 9).unwrap();
    let mut opset = Writer::new();
    opset.write_i64(2, 17).unwrap();
    model.write_message(2, opset.as_slice()).unwrap();
    model.write_message(7, graph_with(&[]).as_slice()).unwrap();
    model.write_message(13, entry.as_slice()).unwrap();
    let model = decode(&model.into_vec()).unwrap();
    assert_eq!(model.metadata, vec![("author".into(), "someone".into())]);
}

// ----------------------------------------------------------------- gates ---

#[test]
fn ir_version_outside_the_supported_range_is_refused() {
    let graph = graph_with(&[(1, node("Relu", &["x"], &["y"], &[]))]);
    for ir in [2i64, 99] {
        let bytes = model_with(&graph, ir, 17);
        assert!(
            matches!(decode(&bytes), Err(OnnxError::UnsupportedIrVersion(found, max)) if found == ir && max == data_binary_onnx::MAX_SUPPORTED_IR_VERSION)
        );
    }
    assert!(decode(&model_with(&graph, data_binary_onnx::MAX_SUPPORTED_IR_VERSION, 17)).is_ok());
}

#[test]
fn model_without_opset_is_refused() {
    let mut model = Writer::new();
    model.write_i64(1, 9).unwrap();
    model.write_message(7, graph_with(&[]).as_slice()).unwrap();
    assert_eq!(decode(&model.into_vec()), Err(OnnxError::MissingOpset));
}

#[test]
fn model_without_graph_is_refused() {
    let mut model = Writer::new();
    model.write_i64(1, 9).unwrap();
    let mut opset = Writer::new();
    opset.write_i64(2, 17).unwrap();
    model.write_message(2, opset.as_slice()).unwrap();
    assert_eq!(decode(&model.into_vec()), Err(OnnxError::MissingGraph));
}

#[test]
fn node_without_op_type_is_refused() {
    let mut node = Writer::new();
    node.write_string(1, "x").unwrap();
    node.write_string(2, "y").unwrap();
    let graph = graph_with(&[(1, node.into_vec())]);
    let bytes = model_with(&graph, 9, 17);
    match decode(&bytes) {
        Err(OnnxError::MissingOpType { .. }) => {}
        other => panic!("expected missing op_type, got {other:?}"),
    }
}

#[test]
fn element_count_mismatch_is_refused_at_import() {
    // Declares a 2x3 tensor but supplies four floats.
    let mut tensor = Writer::new();
    tensor.write_i64(1, 2).unwrap();
    tensor.write_i64(1, 3).unwrap();
    tensor.write_i32(2, DataType::Float.tag()).unwrap();
    tensor.write_string(8, "bad").unwrap();
    tensor.write_bytes(9, &vec![0u8; 16]).unwrap();
    let bytes = model_with(&graph_with(&[(3, tensor.into_vec())]), 9, 17);
    match decode(&bytes) {
        Err(OnnxError::ElementCount { name, declared, actual }) => {
            assert_eq!(name, "bad");
            assert_eq!(declared, 6);
            assert_eq!(actual, 4);
        }
        other => panic!("expected element count failure, got {other:?}"),
    }
}

#[test]
fn both_raw_and_typed_payload_is_refused() {
    let mut tensor = Writer::new();
    tensor.write_i64(1, 1).unwrap();
    tensor.write_i32(2, DataType::Float.tag()).unwrap();
    tensor.write_string(8, "dup").unwrap();
    tensor.write_bytes(9, &1.0f32.to_le_bytes()).unwrap();
    tensor.write_packed(4, &[2.0f32]).unwrap();
    let bytes = model_with(&graph_with(&[(3, tensor.into_vec())]), 9, 17);
    assert!(matches!(decode(&bytes), Err(OnnxError::ConflictingPayload { .. })));
}

#[test]
fn undefined_data_type_tag_is_refused_when_elements_exist() {
    let mut tensor = Writer::new();
    tensor.write_i64(1, 1).unwrap();
    tensor.write_string(8, "mystery").unwrap();
    tensor.write_bytes(9, &1.0f32.to_le_bytes()).unwrap();
    let bytes = model_with(&graph_with(&[(3, tensor.into_vec())]), 9, 17);
    assert!(matches!(decode(&bytes), Err(OnnxError::UnknownDataType { .. })));
}

#[test]
fn a_tag_for_a_type_that_does_not_exist_is_refused() {
    let mut tensor = Writer::new();
    tensor.write_i64(1, 1).unwrap();
    tensor.write_i32(2, 200).unwrap();
    tensor.write_string(8, "future").unwrap();
    let bytes = model_with(&graph_with(&[(3, tensor.into_vec())]), 9, 17);
    assert!(matches!(decode(&bytes), Err(OnnxError::UnknownDataType { data_type: 200, .. })));
}

#[test]
fn negative_dimension_is_refused() {
    let mut tensor = Writer::new();
    tensor.write_i64(1, -2).unwrap();
    tensor.write_i32(2, DataType::Float.tag()).unwrap();
    tensor.write_string(8, "neg").unwrap();
    let bytes = model_with(&graph_with(&[(3, tensor.into_vec())]), 9, 17);
    assert!(matches!(decode(&bytes), Err(OnnxError::NegativeDim(n)) if n == "neg"));
}

#[test]
fn duplicate_attribute_is_refused() {
    let mk = || {
        let mut a = Writer::new();
        a.write_string(1, "axis").unwrap();
        a.write_i32(2, 2).unwrap();
        a.write_i64(4, 1).unwrap();
        a.into_vec()
    };
    let graph = graph_with(&[(1, node("Reduce", &["x"], &["y"], &[mk(), mk()]))]);
    let bytes = model_with(&graph, 9, 17);
    assert!(matches!(decode(&bytes), Err(OnnxError::DuplicateAttribute { .. })));
}

#[test]
fn duplicate_initializer_is_refused() {
    let init = testkit::initializer("W", &[1.0]);
    let graph = graph_with(&[(3, init.clone()), (3, init)]);
    let bytes = model_with(&graph, 9, 17);
    assert!(matches!(decode(&bytes), Err(OnnxError::DuplicateInitializer { initializer, .. }) if initializer == "W"));
}

#[test]
fn attribute_with_no_payload_is_refused() {
    let mut attr = Writer::new();
    attr.write_string(1, "empty").unwrap();
    attr.write_i32(2, 1).unwrap(); // declares FLOAT but carries nothing
    let graph = graph_with(&[(1, node("Op", &["x"], &["y"], &[attr.into_vec()]))]);
    let bytes = model_with(&graph, 9, 17);
    assert!(matches!(decode(&bytes), Err(OnnxError::UndefinedAttribute(n)) if n == "empty"));
}

#[test]
fn external_weights_are_refused_unless_explicitly_allowed() {
    let entry = |key: &str, value: &str| {
        let mut e = Writer::new();
        e.write_string(1, key).unwrap();
        e.write_string(2, value).unwrap();
        e.into_vec()
    };
    let mut tensor = Writer::new();
    tensor.write_i64(1, 4).unwrap();
    tensor.write_i32(2, DataType::Float.tag()).unwrap();
    tensor.write_string(8, "big").unwrap();
    tensor.write_i32(11, 1).unwrap();
    tensor.write_message(10, &entry("location", "/tmp/weights.bin")).unwrap();
    tensor.write_message(10, &entry("offset", "128")).unwrap();
    tensor.write_message(10, &entry("length", "16")).unwrap();
    let bytes = model_with(&graph_with(&[(3, tensor.into_vec())]), 9, 17);

    // Default: a model may not name paths on disk.
    match decode(&bytes) {
        Err(OnnxError::ExternalData { name, location }) => {
            assert_eq!(name, "big");
            assert_eq!(location, "/tmp/weights.bin");
        }
        other => panic!("expected external refusal, got {other:?}"),
    }

    // Opt-in: the reference is reported, never opened.
    let model = decode_with(&bytes, &Config { allow_external_data: true, ..Config::default() }).unwrap();
    let t = model.initializer("big").unwrap();
    assert_eq!(t.payload, Payload::External { location: "/tmp/weights.bin".into() });
    let ext = t.external.as_ref().unwrap();
    assert_eq!((ext.offset, ext.length), (128, 16));
    assert!(ext.checksum.is_none());
}

#[test]
fn data_location_external_without_entries_is_refused() {
    let mut tensor = Writer::new();
    tensor.write_i32(2, DataType::Float.tag()).unwrap();
    tensor.write_string(8, "ghost").unwrap();
    tensor.write_i32(11, 1).unwrap();
    let bytes = model_with(&graph_with(&[(3, tensor.into_vec())]), 9, 17);
    assert!(matches!(decode(&bytes), Err(OnnxError::ExternalData { .. })));
}

#[test]
fn subgraphs_beyond_the_depth_cap_are_refused() {
    fn nested(depth: usize) -> Vec<u8> {
        if depth == 0 {
            let mut leaf = Writer::new();
            leaf.write_string(2, "leaf").unwrap();
            return leaf.into_vec();
        }
        let mut attr = Writer::new();
        attr.write_string(1, "branch").unwrap();
        attr.write_i32(2, 5).unwrap();
        attr.write_message(7, &nested(depth - 1)).unwrap();

        let mut node = Writer::new();
        node.write_string(4, "If").unwrap();
        node.write_message(5, attr.as_slice()).unwrap();

        let mut graph = Writer::new();
        graph.write_string(2, "level").unwrap();
        graph.write_message(1, node.as_slice()).unwrap();
        graph.into_vec()
    }

    let cap = data_binary_onnx::DEFAULT_MAX_SUBGRAPH_DEPTH;
    // One level under the cap still loads.
    assert!(decode(&model_with(&nested(cap - 1), 9, 17)).is_ok());
    assert!(matches!(decode(&model_with(&nested(cap + 1), 9, 17)), Err(OnnxError::TooDeepSubgraphs(d)) if d == cap));
}

#[test]
fn empty_input_is_rejected_as_a_missing_graph() {
    // The graph is what a model without one cannot be, and it is reported
    // before the opset gate so the message names the missing structure.
    assert_eq!(decode(&[]), Err(OnnxError::MissingGraph));
}

// ------------------------------------------------------------- malformed ---

#[test]
fn non_protobuf_input_is_rejected() {
    // "ONNX" ASCII is not a valid message.
    assert!(matches!(decode(b"ONNX"), Err(OnnxError::Wire(_))));
}

#[test]
fn truncated_model_is_rejected() {
    let full = testkit::minimal_model(17);
    for cut in 1..full.len() {
        assert!(decode(&full[..cut]).is_err(), "prefix of {cut} bytes accepted");
    }
}

/// Decoding the same bytes twice gives the same model, which is what lets a
/// cache key be derived from the file's digest.
#[test]
fn decoding_is_deterministic() {
    let bytes = model_with(&graph_with(&[(3, testkit::initializer("W", &[0.5, 1.5])), (1, node("Relu", &["x"], &["y"], &[]))]), 9, 17);
    assert_eq!(decode(&bytes), decode(&bytes));
    assert!(decode(&bytes).unwrap().initializer("W").is_some());
}
