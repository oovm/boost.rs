//! The hand-written ONNX reader.
//!
//! Every message is walked with [`data_binary_protobuf::Reader`] and an
//! explicit field number, so the schema this decoder implements is auditable
//! against `onnx.proto` rather than generated from it. Fields outside the
//! subset modelled here are skipped, which is what lets a model produced by a
//! newer ONNX revision still load as long as the parts a consumer needs are
//! understood.

use data_binary_protobuf::{Reader, Tag, WireType};

use crate::dtype::{AttributeType, DataType, field};
use crate::error::OnnxError;
use crate::model::*;

/// Oldest IR version accepted. Earlier revisions predate the current
/// `AttributeProto.type` discipline and are not worth guessing at.
pub const MIN_SUPPORTED_IR_VERSION: i64 = 3;

/// Newest IR version this decoder claims to interpret. A newer file may add
/// fields whose meaning is unknown here, so it is refused rather than
/// half-read.
pub const MAX_SUPPORTED_IR_VERSION: i64 = 12;

/// Default cap on nested subgraphs reached through `Graph` attributes.
pub const DEFAULT_MAX_SUBGRAPH_DEPTH: usize = 8;

/// `TensorProto.DataLocation.EXTERNAL` — weights live in a sidecar file.
const DATA_LOCATION_EXTERNAL: i32 = 1;

/// Knobs that change what the decoder is willing to accept.
#[derive(Debug, Clone)]
pub struct Config {
    /// Keep sidecar-referenced weights as [`Payload::External`] instead of
    /// failing. Off by default: silently resolving a `location` string would
    /// let a model file name arbitrary paths on disk.
    pub allow_external_data: bool,
    /// How deep `If`/`Loop`/`Scan` subgraphs may nest.
    pub max_subgraph_depth: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config { allow_external_data: false, max_subgraph_depth: DEFAULT_MAX_SUBGRAPH_DEPTH }
    }
}

/// Decode a serialised `ModelProto`.
pub fn decode(bytes: &[u8]) -> Result<Model, OnnxError> {
    decode_with(bytes, &Config::default())
}

/// Decode a `ModelProto` with an explicit policy.
pub fn decode_with(bytes: &[u8], config: &Config) -> Result<Model, OnnxError> {
    let mut model = read_model(&mut Reader::new(bytes), config)?;

    if !(MIN_SUPPORTED_IR_VERSION..=MAX_SUPPORTED_IR_VERSION).contains(&model.ir_version) {
        return Err(OnnxError::UnsupportedIrVersion(model.ir_version, MAX_SUPPORTED_IR_VERSION));
    }
    if model.opset_imports.is_empty() {
        return Err(OnnxError::MissingOpset);
    }

    check_graph(&model.graph)?;
    for node in &model.graph.nodes {
        check_node(node)?;
    }
    Ok(model)
}

// ------------------------------------------------------------------- model ---

fn read_model(reader: &mut Reader<'_>, config: &Config) -> Result<Model, OnnxError> {
    let mut model = Model {
        ir_version: 0,
        opset_imports: Vec::new(),
        producer_name: String::new(),
        producer_version: String::new(),
        domain: String::new(),
        model_version: 0,
        graph: Graph {
            name: String::new(),
            nodes: Vec::new(),
            initializers: Vec::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            value_info: Vec::new(),
        },
        metadata: Vec::new(),
    };
    let mut graph_seen = false;

    while let Some(tag) = reader.next_tag()? {
        match tag.number {
            field::MODEL_IR_VERSION => model.ir_version = read_i64(reader, tag)?,
            field::MODEL_PRODUCER_NAME => model.producer_name = read_string(reader, tag)?,
            field::MODEL_PRODUCER_VERSION => model.producer_version = read_string(reader, tag)?,
            field::MODEL_DOMAIN => model.domain = read_string(reader, tag)?,
            field::MODEL_VERSION => model.model_version = read_i64(reader, tag)?,
            field::MODEL_GRAPH => {
                model.graph = read_graph(&mut reader.read_message(tag.number)?, config, 0)?;
                graph_seen = true;
            }
            field::MODEL_OPSET_IMPORT => {
                model.opset_imports.push(read_opset(&mut reader.read_message(tag.number)?)?);
            }
            field::MODEL_METADATA_PROPS => {
                let mut sub = reader.read_message(tag.number)?;
                let mut key = String::new();
                let mut value = String::new();
                while let Some(entry) = sub.next_tag()? {
                    match entry.number {
                        field::ENTRY_KEY => key = read_string(&mut sub, entry)?,
                        field::ENTRY_VALUE => value = read_string(&mut sub, entry)?,
                        _ => sub.skip(entry)?,
                    }
                }
                model.metadata.push((key, value));
            }
            _ => reader.skip(tag)?,
        }
    }

    if !graph_seen {
        return Err(OnnxError::MissingGraph);
    }
    Ok(model)
}

fn read_opset(reader: &mut Reader<'_>) -> Result<OperatorSetId, OnnxError> {
    let mut import = OperatorSetId { domain: String::new(), version: 0 };
    while let Some(tag) = reader.next_tag()? {
        match tag.number {
            field::OPSET_DOMAIN => import.domain = read_string(reader, tag)?,
            field::OPSET_VERSION => import.version = read_i64(reader, tag)?,
            _ => reader.skip(tag)?,
        }
    }
    Ok(import)
}

// ------------------------------------------------------------------- graph ---

fn read_graph(reader: &mut Reader<'_>, config: &Config, depth: usize) -> Result<Graph, OnnxError> {
    if depth > config.max_subgraph_depth {
        return Err(OnnxError::TooDeepSubgraphs(config.max_subgraph_depth));
    }
    let mut graph = Graph {
        name: String::new(),
        nodes: Vec::new(),
        initializers: Vec::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        value_info: Vec::new(),
    };

    while let Some(tag) = reader.next_tag()? {
        match tag.number {
            field::GRAPH_NAME => graph.name = read_string(reader, tag)?,
            field::GRAPH_NODE => graph.nodes.push(read_node(&mut reader.read_message(tag.number)?, config, depth)?),
            field::GRAPH_INITIALIZER => graph.initializers.push(read_tensor(&mut reader.read_message(tag.number)?, config)?),
            field::GRAPH_INPUT => graph.inputs.push(read_value_info(&mut reader.read_message(tag.number)?)?),
            field::GRAPH_OUTPUT => graph.outputs.push(read_value_info(&mut reader.read_message(tag.number)?)?),
            field::GRAPH_VALUE_INFO => graph.value_info.push(read_value_info(&mut reader.read_message(tag.number)?)?),
            _ => reader.skip(tag)?,
        }
    }
    Ok(graph)
}

/// Reject graphs whose shape a consumer could not act on.
fn check_graph(graph: &Graph) -> Result<(), OnnxError> {
    let mut seen: Vec<&str> = Vec::with_capacity(graph.initializers.len());
    for tensor in &graph.initializers {
        validate_tensor(tensor)?;
        // An unnamed initializer cannot be referenced by any node, but it is
        // not a collision; leave it to the producer to explain.
        if tensor.name.is_empty() {
            continue;
        }
        if seen.contains(&tensor.name.as_str()) {
            return Err(OnnxError::DuplicateInitializer { name: graph.name.clone(), initializer: tensor.name.clone() });
        }
        seen.push(&tensor.name);
    }
    Ok(())
}

fn validate_tensor(tensor: &Tensor) -> Result<(), OnnxError> {
    if tensor.dims.iter().any(|&dim| dim < 0) {
        return Err(OnnxError::NegativeDim(tensor.name.clone()));
    }
    if tensor.data_type_tag != 0 && tensor.data_type.is_none() {
        return Err(OnnxError::UnknownDataType { name: tensor.name.clone(), data_type: tensor.data_type_tag });
    }
    // An external tensor carries no bytes here, so there is nothing to count.
    if matches!(tensor.payload, Payload::External { .. }) {
        return Ok(());
    }
    let declared = tensor.element_count().ok_or_else(|| OnnxError::NegativeDim(tensor.name.clone()))?;
    let actual = tensor.payload_elements().ok_or_else(|| OnnxError::UnknownDataType {
        name: tensor.name.clone(),
        data_type: tensor.data_type_tag,
    })?;
    if declared != actual {
        return Err(OnnxError::ElementCount { name: tensor.name.clone(), declared, actual });
    }
    Ok(())
}

fn check_node(node: &Node) -> Result<(), OnnxError> {
    if node.op_type.is_empty() {
        return Err(OnnxError::MissingOpType { name: node.name.clone() });
    }
    let mut seen: Vec<&str> = Vec::with_capacity(node.attributes.len());
    for attribute in &node.attributes {
        if seen.contains(&attribute.name.as_str()) {
            return Err(OnnxError::DuplicateAttribute { name: node.name.clone(), attribute: attribute.name.clone() });
        }
        seen.push(&attribute.name);
        if attribute.attribute_type() == AttributeType::Undefined {
            return Err(OnnxError::UndefinedAttribute(attribute.name.clone()));
        }
    }
    Ok(())
}

// -------------------------------------------------------------------- node ---

fn read_node(reader: &mut Reader<'_>, config: &Config, depth: usize) -> Result<Node, OnnxError> {
    let mut node = Node {
        op_type: String::new(),
        domain: String::new(),
        name: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        attributes: Vec::new(),
    };

    while let Some(tag) = reader.next_tag()? {
        match tag.number {
            field::NODE_OP_TYPE => node.op_type = read_string(reader, tag)?,
            field::NODE_DOMAIN => node.domain = read_string(reader, tag)?,
            field::NODE_NAME => node.name = read_string(reader, tag)?,
            field::NODE_INPUT => node.inputs.push(read_string(reader, tag)?),
            field::NODE_OUTPUT => node.outputs.push(read_string(reader, tag)?),
            field::NODE_ATTRIBUTE => node.attributes.push(read_attribute(&mut reader.read_message(tag.number)?, config, depth)?),
            _ => reader.skip(tag)?,
        }
    }
    Ok(node)
}

fn read_attribute(reader: &mut Reader<'_>, config: &Config, depth: usize) -> Result<Attribute, OnnxError> {
    let mut name = String::new();
    let mut declared = AttributeType::Undefined;
    // Collect every field that appears, then let the declared type pick the
    // payload. Producers occasionally leave `type` unset on legacy single-value
    // attributes, and the fallback below handles that without guessing.
    let mut float = None;
    let mut int = None;
    let mut string = None;
    let mut tensor = None;
    let mut graph = None;
    let mut type_shape = None;
    let mut floats = Vec::new();
    let mut ints = Vec::new();
    let mut strings = Vec::new();
    let mut tensors = Vec::new();
    let mut graphs = Vec::new();

    while let Some(tag) = reader.next_tag()? {
        match tag.number {
            field::ATTR_NAME => name = read_string(reader, tag)?,
            field::ATTR_TYPE => declared = AttributeType::from_tag(read_i32(reader, tag)?),
            field::ATTR_FLOAT => float = Some(read_f32(reader, tag)?),
            field::ATTR_INT => int = Some(read_i64(reader, tag)?),
            field::ATTR_STRING => string = Some(read_bytes(reader, tag)?),
            field::ATTR_TENSOR => tensor = Some(read_tensor(&mut reader.read_message(tag.number)?, config)?),
            field::ATTR_GRAPH => graph = Some(read_graph(&mut reader.read_message(tag.number)?, config, depth + 1)?),
            field::ATTR_TYPE_PROTO => type_shape = Some(read_type(&mut reader.read_message(tag.number)?)?),
            field::ATTR_FLOATS => push_f32s(reader, tag, &mut floats)?,
            field::ATTR_INTS => push_i64s(reader, tag, &mut ints)?,
            field::ATTR_STRINGS => strings.push(read_bytes(reader, tag)?),
            field::ATTR_TENSORS => tensors.push(read_tensor(&mut reader.read_message(tag.number)?, config)?),
            field::ATTR_GRAPHS => graphs.push(read_graph(&mut reader.read_message(tag.number)?, config, depth + 1)?),
            _ => reader.skip(tag)?,
        }
    }

    let kind = match declared {
        AttributeType::Float => float.map(AttributeKind::Float),
        AttributeType::Int => int.map(AttributeKind::Int),
        AttributeType::String => string.map(AttributeKind::String),
        AttributeType::Tensor => tensor.map(|t| AttributeKind::Tensor(Box::new(t))),
        AttributeType::Graph => graph.map(|g| AttributeKind::Graph(Box::new(g))),
        AttributeType::TypeProto => type_shape.map(AttributeKind::Type),
        AttributeType::Floats => Some(AttributeKind::Floats(floats)),
        AttributeType::Ints => Some(AttributeKind::Ints(ints)),
        AttributeType::Strings => Some(AttributeKind::Strings(strings)),
        AttributeType::Tensors => Some(AttributeKind::Tensors(tensors)),
        AttributeType::Graphs => Some(AttributeKind::Graphs(graphs)),
        // Legacy attributes sometimes omit `type`; infer from what arrived.
        AttributeType::Undefined => {
            if !floats.is_empty() {
                Some(AttributeKind::Floats(floats))
            } else if !ints.is_empty() {
                Some(AttributeKind::Ints(ints))
            } else if !strings.is_empty() {
                Some(AttributeKind::Strings(strings))
            } else if !tensors.is_empty() {
                Some(AttributeKind::Tensors(tensors))
            } else if !graphs.is_empty() {
                Some(AttributeKind::Graphs(graphs))
            } else {
                float.map(AttributeKind::Float).or_else(|| int.map(AttributeKind::Int)).or_else(|| string.map(AttributeKind::String))
            }
        }
        AttributeType::SparseTensor | AttributeType::SparseTensors | AttributeType::TypeProtos => None,
    };

    let kind = kind.ok_or_else(|| OnnxError::UndefinedAttribute(name.clone()))?;

    Ok(Attribute { name, kind })
}

// ------------------------------------------------------------------ tensor ---

fn read_tensor(reader: &mut Reader<'_>, config: &Config) -> Result<Tensor, OnnxError> {
    let mut name = String::new();
    let mut data_type_tag = 0i32;
    let mut dims = Vec::new();
    let mut raw: Option<Vec<u8>> = None;
    let mut strings: Vec<Vec<u8>> = Vec::new();
    let mut external: Vec<(String, String)> = Vec::new();
    let mut data_location = 0i32;
    // Typed lists are gathered as bit patterns so one encoding path can widen
    // or narrow them to whatever `data_type` says.
    let mut typed: Option<(u32, Vec<u64>)> = None;

    while let Some(tag) = reader.next_tag()? {
        match tag.number {
            field::TENSOR_NAME => name = read_string(reader, tag)?,
            field::TENSOR_DATA_TYPE => data_type_tag = read_i32(reader, tag)?,
            field::TENSOR_DIMS => push_i64s(reader, tag, &mut dims)?,
            field::TENSOR_RAW_DATA => raw = Some(read_bytes(reader, tag)?),
            field::TENSOR_STRING_DATA => strings.push(read_bytes(reader, tag)?),
            field::TENSOR_DATA_LOCATION => data_location = read_i32(reader, tag)?,
            field::TENSOR_EXTERNAL_DATA => {
                let mut sub = reader.read_message(tag.number)?;
                let mut key = String::new();
                let mut value = String::new();
                while let Some(entry) = sub.next_tag()? {
                    match entry.number {
                        field::ENTRY_KEY => key = read_string(&mut sub, entry)?,
                        field::ENTRY_VALUE => value = read_string(&mut sub, entry)?,
                        _ => sub.skip(entry)?,
                    }
                }
                external.push((key, value));
            }
            field::TENSOR_FLOAT_DATA | field::TENSOR_INT32_DATA | field::TENSOR_INT64_DATA | field::TENSOR_UINT64_DATA => {
                let values = read_typed_list(reader, tag)?;
                match &mut typed {
                    Some((seen, acc)) => {
                        if *seen != tag.number {
                            return Err(OnnxError::ConflictingPayload { name });
                        }
                        acc.extend(values);
                    }
                    None => typed = Some((tag.number, values)),
                }
            }
            _ => reader.skip(tag)?,
        }
    }

    let data_type = DataType::from_tag(data_type_tag);
    let has_typed = typed.is_some();
    let has_strings = !strings.is_empty();
    let has_external = !external.is_empty();

    // Exactly one storage form may carry the elements. `data_location` says
    // which form the producer intended, and an external tensor is only kept as
    // a reference when the caller opted in.
    let forms = usize::from(raw.is_some()) + usize::from(has_typed) + usize::from(has_strings) + usize::from(has_external);
    if forms > 1 {
        return Err(OnnxError::ConflictingPayload { name });
    }
    if has_external && !config.allow_external_data {
        return Err(OnnxError::ExternalData { name, location: external_location(&external) });
    }
    // `data_location == EXTERNAL` promises a sidecar. With no entries the file
    // is self-contradictory, so say so instead of reporting an empty tensor.
    if data_location == DATA_LOCATION_EXTERNAL && !has_external {
        return Err(OnnxError::ExternalData { name, location: String::new() });
    }

    let payload = match (raw, typed) {
        (Some(bytes), _) => Payload::Bytes(bytes),
        (None, Some((_, values))) => {
            // A typed list may be wider than the element type (producers pack
            // fp16 into `int32_data`), so re-slice to the declared width.
            let width = data_type.ok_or(OnnxError::UnknownDataType { name: name.clone(), data_type: data_type_tag })?.bytes_per_element();
            if width == 0 {
                return Err(OnnxError::UnknownDataType { name: name.clone(), data_type: data_type_tag });
            }
            let mut bytes = Vec::with_capacity(values.len() * width);
            for value in values {
                bytes.extend_from_slice(&value.to_le_bytes()[..width]);
            }
            Payload::Bytes(bytes)
        }
        (None, None) if has_strings => Payload::Strings(strings),
        (None, None) if has_external => Payload::External { location: external_location(&external) },
        (None, None) => Payload::Empty,
    };

    Ok(Tensor { name, data_type, data_type_tag, dims, payload, external: parse_external(&external) })
}

fn external_location(entries: &[(String, String)]) -> String {
    entries.iter().find(|(key, _)| key == "location").map(|(_, value)| value.clone()).unwrap_or_default()
}

fn parse_external(entries: &[(String, String)]) -> Option<ExternalData> {
    if entries.is_empty() {
        return None;
    }
    let get = |wanted: &str| entries.iter().find(|(key, _)| key == wanted).map(|(_, value)| value.clone());
    let number = |wanted: &str| get(wanted).and_then(|text| text.parse::<u64>().ok()).unwrap_or_default();
    Some(ExternalData {
        location: get("location").unwrap_or_default(),
        offset: number("offset"),
        length: number("length"),
        checksum: get("checksum"),
    })
}

// -------------------------------------------------------------- value info ---

fn read_value_info(reader: &mut Reader<'_>) -> Result<ValueInfo, OnnxError> {
    let mut name = String::new();
    let mut type_ = TypeShape::default();
    while let Some(tag) = reader.next_tag()? {
        match tag.number {
            field::VALUE_NAME => name = read_string(reader, tag)?,
            field::VALUE_TYPE => type_ = read_type(&mut reader.read_message(tag.number)?)?,
            _ => reader.skip(tag)?,
        }
    }
    Ok(ValueInfo { name, type_ })
}

fn read_type(reader: &mut Reader<'_>) -> Result<TypeShape, OnnxError> {
    let mut shape = TypeShape::default();
    while let Some(tag) = reader.next_tag()? {
        match tag.number {
            field::TYPE_TENSOR | field::TYPE_SPARSE_TENSOR => {
                if tag.number == field::TYPE_SPARSE_TENSOR {
                    shape.non_tensor = Some(NonTensor::SparseTensor);
                }
                let mut inner = reader.read_message(tag.number)?;
                while let Some(field_tag) = inner.next_tag()? {
                    match field_tag.number {
                        field::TENSOR_TYPE_ELEM_TYPE => shape.elem_type = Some(inner.read_i32()?),
                        field::TENSOR_TYPE_SHAPE => shape.dims = read_shape(&mut inner.read_message(field_tag.number)?)?,
                        _ => inner.skip(field_tag)?,
                    }
                }
            }
            field::TYPE_SEQUENCE => {
                shape.non_tensor = Some(NonTensor::Sequence);
                reader.skip(tag)?;
            }
            field::TYPE_MAP => {
                shape.non_tensor = Some(NonTensor::Map);
                reader.skip(tag)?;
            }
            _ => reader.skip(tag)?,
        }
    }
    Ok(shape)
}

fn read_shape(reader: &mut Reader<'_>) -> Result<Vec<Option<Dimension>>, OnnxError> {
    let mut dims = Vec::new();
    while let Some(tag) = reader.next_tag()? {
        if tag.number != field::SHAPE_DIM {
            reader.skip(tag)?;
            continue;
        }
        let mut dim = reader.read_message(tag.number)?;
        let mut value = None;
        let mut param = None;
        while let Some(axis) = dim.next_tag()? {
            match axis.number {
                field::DIM_VALUE => value = Some(dim.read_i64()?),
                field::DIM_PARAM => param = Some(dim.read_string(axis.number)?.to_owned()),
                _ => dim.skip(axis)?,
            }
        }
        dims.push(value.map(Dimension::Known).or(param.map(Dimension::Symbolic)));
    }
    Ok(dims)
}

// ----------------------------------------------------------------- helpers ---

/// Read one varint field as `i64`.
fn read_i64(reader: &mut Reader<'_>, tag: Tag) -> Result<i64, OnnxError> {
    expect(tag, WireType::Varint)?;
    Ok(reader.read_i64()?)
}

fn read_i32(reader: &mut Reader<'_>, tag: Tag) -> Result<i32, OnnxError> {
    expect(tag, WireType::Varint)?;
    Ok(reader.read_i32()?)
}

fn read_f32(reader: &mut Reader<'_>, tag: Tag) -> Result<f32, OnnxError> {
    if tag.wire != WireType::Fixed32 {
        return Err(OnnxError::Wire(format!("field {} expects a 4-byte value", tag.number)));
    }
    Ok(reader.read_float()?)
}

fn read_string(reader: &mut Reader<'_>, tag: Tag) -> Result<String, OnnxError> {
    if tag.wire != WireType::LengthDelimited {
        return Err(OnnxError::Wire(format!("field {} expects a length-delimited value", tag.number)));
    }
    Ok(reader.read_string(tag.number)?.to_owned())
}

fn read_bytes(reader: &mut Reader<'_>, tag: Tag) -> Result<Vec<u8>, OnnxError> {
    if tag.wire != WireType::LengthDelimited {
        return Err(OnnxError::Wire(format!("field {} expects a length-delimited value", tag.number)));
    }
    Ok(reader.read_bytes(tag.number)?.to_vec())
}

fn expect(tag: Tag, want: WireType) -> Result<(), OnnxError> {
    if tag.wire != want {
        return Err(OnnxError::Wire(format!("field {} expects wire type {want:?}, found {:?}", tag.number, tag.wire)));
    }
    Ok(())
}

/// Append one occurrence of a repeated `int64`.
///
/// A packed run arrives as one length-delimited tag, a non-packed element as
/// one varint tag per value. Both appear in real files, so both are accepted.
fn push_i64s(reader: &mut Reader<'_>, tag: Tag, out: &mut Vec<i64>) -> Result<(), OnnxError> {
    match tag.wire {
        WireType::LengthDelimited => {
            for value in reader.read_packed::<i64>(tag)? {
                out.push(value?);
            }
        }
        WireType::Varint => out.push(reader.read_i64()?),
        other => return Err(OnnxError::Wire(format!("field {} cannot hold a {other:?} as int64", tag.number))),
    }
    Ok(())
}

fn push_f32s(reader: &mut Reader<'_>, tag: Tag, out: &mut Vec<f32>) -> Result<(), OnnxError> {
    match tag.wire {
        WireType::LengthDelimited => {
            for value in reader.read_packed::<f32>(tag)? {
                out.push(value?);
            }
        }
        WireType::Fixed32 => out.push(reader.read_float()?),
        other => return Err(OnnxError::Wire(format!("field {} cannot hold a {other:?} as float", tag.number))),
    }
    Ok(())
}

/// Read a typed data list as raw bit patterns.
///
/// `float_data` and `int32_data` are 32-bit, `int64_data` and `uint64_data` are
/// 64-bit; the caller re-slices them to the tensor's element width.
fn read_typed_list(reader: &mut Reader<'_>, tag: Tag) -> Result<Vec<u64>, OnnxError> {
    let mut out = Vec::new();
    match tag.wire {
        WireType::LengthDelimited => match tag.number {
            field::TENSOR_FLOAT_DATA => {
                for value in reader.read_packed::<f32>(tag)? {
                    out.push(f32::to_bits(value?) as u64);
                }
            }
            field::TENSOR_INT32_DATA => {
                for value in reader.read_packed::<u32>(tag)? {
                    out.push(u64::from(value?));
                }
            }
            field::TENSOR_INT64_DATA => {
                for value in reader.read_packed::<i64>(tag)? {
                    out.push(value? as u64);
                }
            }
            _ => {
                for value in reader.read_packed::<u64>(tag)? {
                    out.push(value?);
                }
            }
        },
        WireType::Fixed32 => out.push(u64::from(reader.read_fixed32()?)),
        WireType::Varint | WireType::Fixed64 => out.push(reader.read_varint()?),
        other => return Err(OnnxError::Wire(format!("field {} cannot hold a {other:?} as data", tag.number))),
    }
    Ok(out)
}
