//! 按 TypeTree 把对象字节解码为 `serde_json::Value`。
//!
//! 语义对齐 Unity 的 TypeTree 反序列化规则：标量读完后按 `m_MetaFlag` 对齐，
//! 字符串始终对齐 4 字节，数组/映射在读取后按节点标志对齐，结构体读完所有
//! 子字段后对齐。

use serde_json::{Map, Number, Value};

use crate::{
    UnityError,
    reader::ByteReader,
    typetree::{Kind, TypeTree},
};

const MAX_VALUE_DEPTH: usize = 128;

const REFERENCED_TYPES: [&str; 3] = ["ReferencedObject", "ReferencedObjectData", "ManagedReferencesRegistry"];

/// 解码一个对象。
pub(crate) fn decode_value(typetree: &TypeTree, bytes: &[u8], little: bool) -> Result<Value, UnityError> {
    let mut reader = ByteReader::new(bytes);
    reader.set_little(little);
    decode_node(typetree, &mut reader, 0)
}

fn decode_node(typetree: &TypeTree, reader: &mut ByteReader<'_>, depth: usize) -> Result<Value, UnityError> {
    if depth > MAX_VALUE_DEPTH {
        return Err(UnityError::Parse("object nesting is too deep to decode".into()));
    }
    if REFERENCED_TYPES.contains(&typetree.m_type.as_str()) {
        return Err(UnityError::Unsupported(format!("unsupported referenced object type `{}`", typetree.m_type)));
    }

    match typetree.classify() {
        Kind::Bool => {
            let value = reader.read_u8()? != 0;
            align_if_needed(typetree, reader)?;
            Ok(Value::Bool(value))
        }
        Kind::U8 => {
            let value = reader.read_u8()?;
            align_if_needed(typetree, reader)?;
            Ok(Value::Number(Number::from(value)))
        }
        Kind::U16 => {
            let value = reader.read_u16()?;
            align_if_needed(typetree, reader)?;
            Ok(Value::Number(Number::from(value)))
        }
        Kind::U32 => {
            let value = reader.read_u32()?;
            align_if_needed(typetree, reader)?;
            Ok(Value::Number(Number::from(value)))
        }
        Kind::U64 => {
            let value = reader.read_u64()?;
            align_if_needed(typetree, reader)?;
            Ok(Value::Number(Number::from(value)))
        }
        Kind::I8 => {
            let value = reader.read_u8()? as i8;
            align_if_needed(typetree, reader)?;
            Ok(Value::Number(Number::from(value)))
        }
        Kind::I16 => {
            let value = reader.read_i16()?;
            align_if_needed(typetree, reader)?;
            Ok(Value::Number(Number::from(value)))
        }
        Kind::I32 => {
            let value = reader.read_i32()?;
            align_if_needed(typetree, reader)?;
            Ok(Value::Number(Number::from(value)))
        }
        Kind::I64 => {
            let value = reader.read_i64()?;
            align_if_needed(typetree, reader)?;
            Ok(Value::Number(Number::from(value)))
        }
        Kind::Float => {
            let value = reader.read_f32()?;
            align_if_needed(typetree, reader)?;
            Ok(float_value(value as f64))
        }
        Kind::Double => {
            let value = reader.read_f64()?;
            align_if_needed(typetree, reader)?;
            Ok(float_value(value))
        }
        Kind::Char => {
            let value = reader.read_u8()?;
            align_if_needed(typetree, reader)?;
            Ok(Value::String(char::from(value).to_string()))
        }
        Kind::String => decode_string(reader),
        Kind::Map => decode_map(typetree, reader, depth),
        Kind::Untyped => decode_bytes_array(reader),
        Kind::Empty | Kind::TodoType => Ok(Value::Null),
        Kind::ArrayWrapper | Kind::Array => decode_array(typetree, reader, depth),
        Kind::Struct => decode_struct(typetree, reader, depth),
    }
}

fn align_if_needed(typetree: &TypeTree, reader: &mut ByteReader<'_>) -> Result<(), UnityError> {
    if typetree.requires_align() {
        reader.align(4)?;
    }
    Ok(())
}

fn float_value(value: f64) -> Value {
    Number::from_f64(value).map_or(Value::Null, Value::Number)
}

fn decode_string(reader: &mut ByteReader<'_>) -> Result<Value, UnityError> {
    let len = reader.read_u32()? as usize;
    let bytes = reader.read_bytes(len)?;
    let value = String::from_utf8_lossy(bytes).into_owned();
    reader.align(4)?;
    Ok(Value::String(value))
}

fn decode_bytes_array(reader: &mut ByteReader<'_>) -> Result<Value, UnityError> {
    let len = reader.read_u32()? as usize;
    let bytes = reader.read_bytes(len)?;
    Ok(Value::Array(bytes.iter().map(|&byte| Value::Number(Number::from(byte))).collect()))
}

fn decode_array(typetree: &TypeTree, reader: &mut ByteReader<'_>, depth: usize) -> Result<Value, UnityError> {
    if typetree.m_type == "TypelessData" {
        return decode_bytes_array(reader);
    }

    let array = match typetree.children.as_slice() {
        [child] if child.m_type == "Array" => child,
        _ => {
            return Err(UnityError::Parse("array typetree node has no Array child".into()));
        }
    };
    let item_type = array.children.get(1).ok_or_else(|| UnityError::Parse("array typetree node has no element type".into()))?;

    let len = read_collection_len(reader, "array")?;
    let mut items = Vec::with_capacity(len.min(1024));
    for _ in 0..len {
        items.push(decode_node(item_type, reader, depth + 1)?);
    }

    if typetree.requires_align() || array.requires_align() {
        reader.align(4)?;
    }
    Ok(Value::Array(items))
}

fn decode_map(typetree: &TypeTree, reader: &mut ByteReader<'_>, depth: usize) -> Result<Value, UnityError> {
    let pair = typetree
        .children
        .first()
        .and_then(|array| array.children.get(1))
        .ok_or_else(|| UnityError::Parse("map typetree node has no pair type".into()))?;
    let key_type = pair.children.first().ok_or_else(|| UnityError::Parse("map pair has no key type".into()))?;
    let value_type = pair.children.get(1).ok_or_else(|| UnityError::Parse("map pair has no value type".into()))?;

    let len = read_collection_len(reader, "map")?;
    let mut map = Map::new();
    for _ in 0..len {
        let key = match key_type.classify() {
            Kind::U32 => {
                let value = reader.read_u32()?;
                if key_type.requires_align() {
                    reader.align(4)?;
                }
                value.to_string()
            }
            Kind::String => match decode_node(key_type, reader, depth + 1)? {
                Value::String(value) => value,
                _ => {
                    return Err(UnityError::Parse("map string key did not decode to a string".into()));
                }
            },
            other => {
                return Err(UnityError::Unsupported(format!("unsupported map key type {other:?}")));
            }
        };
        let value = decode_node(value_type, reader, depth + 1)?;
        map.insert(key, value);
    }

    if typetree.requires_align() || pair.requires_align() {
        reader.align(4)?;
    }
    Ok(Value::Object(map))
}

fn decode_struct(typetree: &TypeTree, reader: &mut ByteReader<'_>, depth: usize) -> Result<Value, UnityError> {
    let mut map = Map::new();
    for child in &typetree.children {
        let value = decode_node(child, reader, depth + 1)?;
        map.insert(child.m_name.clone(), value);
    }
    if typetree.requires_align() {
        reader.align(4)?;
    }
    Ok(Value::Object(map))
}

fn read_collection_len(reader: &mut ByteReader<'_>, what: &str) -> Result<usize, UnityError> {
    let len = reader.read_u32()? as usize;
    if len > reader.remaining() {
        return Err(UnityError::Parse(format!("{what} length {len} exceeds remaining bytes {}", reader.remaining())));
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(ty: &str, name: &str, children: Vec<TypeTree>) -> TypeTree {
        TypeTree { m_type: ty.to_string(), m_name: name.to_string(), children, ..Default::default() }
    }

    fn array_wrapper(item: TypeTree) -> TypeTree {
        node("vector", "items", vec![node("Array", "Array", vec![node("int", "size", Vec::new()), item])])
    }

    #[test]
    fn decodes_scalar_with_alignment() {
        let typetree = node("Holder", "Base", vec![TypeTree { m_meta_flag: 0x4000, ..node("UInt8", "value", Vec::new()) }]);
        let value = decode_value(&typetree, &[5, 0, 0, 0], true).unwrap();
        assert_eq!(value, serde_json::json!({"value": 5}));
    }

    #[test]
    fn decodes_arrays() {
        let typetree = array_wrapper(node("UInt32", "data", Vec::new()));
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        for value in [1u32, 2, 3] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        assert_eq!(decode_value(&typetree, &bytes, true).unwrap(), serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn decodes_string_keyed_maps() {
        let pair = node("pair", "data", vec![node("string", "first", Vec::new()), node("UInt32", "second", Vec::new())]);
        let typetree = node("map", "values", vec![node("Array", "Array", vec![node("int", "size", Vec::new()), pair])]);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&2u32.to_le_bytes());

        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(b"AB");
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&7u32.to_le_bytes());

        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(b"C");
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes.extend_from_slice(&9u32.to_le_bytes());

        assert_eq!(decode_value(&typetree, &bytes, true).unwrap(), serde_json::json!({"AB": 7, "C": 9}));
    }

    #[test]
    fn rejects_oversized_and_truncated_collections() {
        let typetree = array_wrapper(node("UInt32", "data", Vec::new()));
        assert!(decode_value(&typetree, &100u32.to_le_bytes(), true).is_err());
        assert!(decode_value(&typetree, &[2, 0, 0, 0, 1, 0, 0, 0], true).is_err());
    }
}
