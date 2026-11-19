use crate::{
    ast::{TomlRoot, TomlValueNode},
    language::{TomlArray, TomlTable, TomlValue},
};
use data_text_core::TextError;
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use std::{collections::HashMap, fmt::Formatter};

/// TOML 反序列化实现
pub fn deserialize<'de, D>(deserializer: D) -> Result<TomlValue, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(TomlDeserializer {})
}

struct TomlDeserializer {}

impl<'de> Visitor<'de> for TomlDeserializer {
    type Value = TomlValue;

    fn expecting(&self, formatter: &mut Formatter) -> std::fmt::Result {
        formatter.write_str("a valid TOML value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::Boolean(v))
    }

    fn visit_i8<E>(self, v: i8) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::Integer(v as i64))
    }

    fn visit_i16<E>(self, v: i16) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::Integer(v as i64))
    }

    fn visit_i32<E>(self, v: i32) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::Integer(v as i64))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::Integer(v))
    }

    fn visit_u8<E>(self, v: u8) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::Integer(v as i64))
    }

    fn visit_u16<E>(self, v: u16) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::Integer(v as i64))
    }

    fn visit_u32<E>(self, v: u32) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::Integer(v as i64))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::Integer(v as i64))
    }

    fn visit_f32<E>(self, v: f32) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::Float(v as f64))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::Float(v))
    }

    fn visit_char<E>(self, v: char) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::String(v.to_string()))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::String(v.to_string()))
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::String(v))
    }

    fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::String(String::from_utf8_lossy(v).to_string()))
    }

    fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TomlValue::String(String::from_utf8_lossy(&v).to_string()))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        // TOML doesn't have a null type, so we'll use a string "null"
        Ok(TomlValue::String("null".to_string()))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        // TOML doesn't have a unit type, so we'll use a string "null"
        Ok(TomlValue::String("null".to_string()))
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut items = Vec::new();
        while let Some(elem) = seq.next_element()? {
            items.push(elem);
        }
        Ok(TomlValue::Array(TomlArray { list: items }))
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut entries = HashMap::new();
        while let Some((key, value)) = map.next_entry()? {
            entries.insert(key, value);
        }
        Ok(TomlValue::Table(TomlTable { dict: entries }))
    }

    fn visit_enum<A>(self, _variant: A) -> Result<Self::Value, A::Error>
    where
        A: de::EnumAccess<'de>,
    {
        Err(de::Error::custom("enums are not supported"))
    }
}

/// Deserializes a TOML string into a Rust type.
pub fn from_str<'de, T>(toml: &'de str) -> Result<T, TextError>
where
    T: serde::Deserialize<'de>,
{
    // 首先使用 data-text-toml 解析器解析 TOML 字符串
    let mut toml_value = match crate::parse(toml) {
        Ok(root) => root_into_value(root),
        Err(_) => empty_table(),
    };

    // 顶层不是一个 TOML 文档（例如标量、数组或内联表）时，按单个值解析。
    if toml_value.as_table().map_or(true, |table| table.is_empty()) && !toml.trim().is_empty() {
        let wrapped = format!("__value__ = {toml}");
        let wrapped_value = root_into_value(crate::parse(&wrapped)?);
        if let Some(inner) = wrapped_value.get("__value__") {
            toml_value = inner.clone();
        }
    }

    // 使用自定义反序列化器将 Value 转换为目标类型
    let deserializer = ValueDeserializerWrapper::new(toml_value);
    T::deserialize(deserializer)
}

/// A segment of a path into the document value tree.
#[derive(Clone)]
enum PathSeg {
    Key(String),
    Index(usize),
}

/// Splits a parsed key into its component names.
fn key_segments(key: &crate::ast::TomlKey) -> Vec<String> {
    key.segments
        .iter()
        .map(|segment| match segment {
            crate::ast::TomlKeySegment::Bare(bare) => bare.name.clone(),
            crate::ast::TomlKeySegment::Quoted(quoted) => quoted.value.clone(),
        })
        .collect()
}

fn empty_table() -> TomlValue {
    TomlValue::Table(TomlTable { dict: HashMap::new() })
}

/// Walks `path`, creating tables as needed and descending into the last element of arrays of tables.
///
/// A `Key` followed by an `Index` resolves to the array itself, so the index can pick one
/// element; otherwise an existing array of tables is traversed through its last element.
fn ensure_table<'a>(mut node: &'a mut TomlValue, path: &[PathSeg]) -> &'a mut TomlValue {
    for (position, segment) in path.iter().enumerate() {
        let next_is_index = matches!(path.get(position + 1), Some(PathSeg::Index(_)));
        node = match segment {
            PathSeg::Key(key) => {
                if !matches!(node, TomlValue::Table(_)) {
                    *node = empty_table();
                }
                let table = match node {
                    TomlValue::Table(table) => table,
                    _ => unreachable!("node was just normalized to a table"),
                };
                let entry: &'a mut TomlValue = table.dict.entry(key.clone()).or_insert_with(empty_table);
                if next_is_index {
                    entry
                }
                else if matches!(entry, TomlValue::Array(_)) {
                    if let TomlValue::Array(array) = entry {
                        if array.list.is_empty() {
                            array.list.push(empty_table());
                        }
                        array.list.last_mut().expect("array is non-empty")
                    }
                    else {
                        unreachable!("entry was just checked to be an array")
                    }
                }
                else {
                    entry
                }
            }
            PathSeg::Index(index) => {
                if !matches!(node, TomlValue::Array(_)) {
                    *node = TomlValue::Array(TomlArray { list: Vec::new() });
                }
                let array = match node {
                    TomlValue::Array(array) => array,
                    _ => unreachable!("node was just normalized to an array"),
                };
                while array.list.len() <= *index {
                    array.list.push(empty_table());
                }
                &mut array.list[*index]
            }
        };
    }
    node
}

/// Appends a new table to the array of tables at `path`, returning its index.
fn push_table_into_array(base: &mut TomlValue, path: &[String]) -> usize {
    let (last, parents) = path.split_last().expect("array of tables path must not be empty");
    let parent_path: Vec<PathSeg> = parents.iter().cloned().map(PathSeg::Key).collect();
    let parent = ensure_table(base, &parent_path);
    let table = match parent {
        TomlValue::Table(table) => table,
        _ => unreachable!("ensure_table always returns a table"),
    };

    let entry = table.dict.entry(last.clone()).or_insert_with(|| TomlValue::Array(TomlArray { list: Vec::new() }));
    if !matches!(entry, TomlValue::Array(_)) {
        *entry = TomlValue::Array(TomlArray { list: Vec::new() });
    }
    let array = match entry {
        TomlValue::Array(array) => array,
        _ => unreachable!("entry was just normalized to an array"),
    };
    array.list.push(empty_table());
    array.list.len() - 1
}

/// Inserts `value` under `key_segments`, relative to `current`.
fn insert_key_value(base: &mut TomlValue, current: &[PathSeg], key_segments: Vec<String>, value: TomlValue) {
    let mut path = current.to_vec();
    let (last, parents) = key_segments.split_last().expect("key must not be empty");
    path.extend(parents.iter().cloned().map(PathSeg::Key));
    let parent = ensure_table(base, &path);
    if let TomlValue::Table(table) = parent {
        table.dict.insert(last.clone(), value);
    }
}

/// 将 TomlRoot 转换为 Value
fn root_into_value(root: TomlRoot) -> TomlValue {
    let mut value = empty_table();
    let mut current: Vec<PathSeg> = Vec::new();

    for item in root.items {
        match item {
            crate::ast::TomlItem::KeyValue(kv) => {
                let segments = key_segments(&kv.key);
                let node = toml_value_node_into_value(kv.value);
                insert_key_value(&mut value, &current, segments, node);
            }
            crate::ast::TomlItem::Table(table) => {
                let segments = key_segments(&table.header.key);
                let path: Vec<PathSeg> = segments.iter().cloned().map(PathSeg::Key).collect();
                ensure_table(&mut value, &path);

                for kv in table.items {
                    let key = key_segments(&kv.key);
                    let node = toml_value_node_into_value(kv.value);
                    insert_key_value(&mut value, &path, key, node);
                }
                current = path;
            }
            crate::ast::TomlItem::ArrayOfTables(aot) => {
                let segments = key_segments(&aot.header.key);
                let index = push_table_into_array(&mut value, &segments);

                let mut path: Vec<PathSeg> = segments.iter().cloned().map(PathSeg::Key).collect();
                path.push(PathSeg::Index(index));
                for kv in aot.items {
                    let key = key_segments(&kv.key);
                    let node = toml_value_node_into_value(kv.value);
                    insert_key_value(&mut value, &path, key, node);
                }
                current = path;
            }
        }
    }

    value
}

/// 将 TomlValueNode 转换为 Value
fn toml_value_node_into_value(value: TomlValueNode) -> TomlValue {
    match value {
        TomlValueNode::String(s) => TomlValue::String(s.value),
        TomlValueNode::Integer(i) => TomlValue::Integer(i.value),
        TomlValueNode::Float(f) => TomlValue::Float(f.value),
        TomlValueNode::Boolean(b) => TomlValue::Boolean(b.value),
        TomlValueNode::DateTime(dt) => TomlValue::DateTime(dt.value),
        TomlValueNode::Array(a) => {
            let items = a.items.into_iter().map(toml_value_node_into_value).collect();
            TomlValue::Array(TomlArray { list: items })
        }
        TomlValueNode::InlineTable(t) => {
            let mut table = empty_table();
            for kv in t.items {
                let segments = key_segments(&kv.key);
                let node = toml_value_node_into_value(kv.value);
                insert_key_value(&mut table, &[], segments, node);
            }
            table
        }
    }
}

struct ValueDeserializerWrapper {
    value: TomlValue,
}

impl ValueDeserializerWrapper {
    fn new(value: TomlValue) -> Self {
        Self { value }
    }
}

impl<'de> serde::de::Deserializer<'de> for ValueDeserializerWrapper {
    type Error = TextError;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match self.value {
            TomlValue::String(s) => visitor.visit_str(&s),
            TomlValue::Integer(i) => visitor.visit_i64(i),
            TomlValue::Float(f) => visitor.visit_f64(f),
            TomlValue::Boolean(b) => visitor.visit_bool(b),
            TomlValue::DateTime(dt) => visitor.visit_str(&dt),
            TomlValue::Array(TomlArray { list: seq }) => visitor.visit_seq(ValueArrayAccess::new(seq)),
            TomlValue::Table(TomlTable { dict: map }) => visitor.visit_map(ValueMapAccess::new(map)),
        }
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_i8<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_i16<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_i32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_i64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_u8<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_u16<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_u32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_u64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_f32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_f64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_char<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match self.value {
            TomlValue::String(ref s) if s == "null" => visitor.visit_none(),
            value => visitor.visit_some(ValueDeserializerWrapper::new(value)),
        }
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_unit_struct<V>(self, _name: &'static str, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_newtype_struct<V>(self, _name: &'static str, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple<V>(self, _len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple_struct<V>(self, _name: &'static str, _len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_struct<V>(self, _name: &'static str, _fields: &'static [&'static str], visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_enum<V>(self, _name: &'static str, _variants: &'static [&'static str], _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(TextError::custom_error("Enums are not supported"))
    }

    fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }
}

struct ValueArrayAccess {
    items: Vec<TomlValue>,
    index: usize,
}

impl ValueArrayAccess {
    fn new(items: Vec<TomlValue>) -> Self {
        Self { items, index: 0 }
    }
}

impl<'de> SeqAccess<'de> for ValueArrayAccess {
    type Error = TextError;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        if self.index < self.items.len() {
            let value = self.items[self.index].clone();
            self.index += 1;
            seed.deserialize(ValueDeserializerWrapper::new(value)).map(Some)
        }
        else {
            Ok(None)
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len() - self.index)
    }
}

struct ValueMapAccess {
    entries: Vec<(String, TomlValue)>,
    index: usize,
}

impl ValueMapAccess {
    fn new(map: HashMap<String, TomlValue>) -> Self {
        Self { entries: map.into_iter().collect(), index: 0 }
    }
}

impl<'de> MapAccess<'de> for ValueMapAccess {
    type Error = TextError;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: DeserializeSeed<'de>,
    {
        if self.index < self.entries.len() {
            let key = &self.entries[self.index].0;
            seed.deserialize(serde::de::value::StrDeserializer::new(key)).map(Some)
        }
        else {
            Ok(None)
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: DeserializeSeed<'de>,
    {
        let value = self.entries[self.index].1.clone();
        self.index += 1;
        seed.deserialize(ValueDeserializerWrapper::new(value))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len() - self.index)
    }
}
