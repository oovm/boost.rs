use serde::ser::{Serialize, Serializer};

use crate::language::{TomlArray, TomlTable, TomlValue};
use data_text_core::TextError;

/// TOML 序列化实现
pub fn serialize<S, T>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Serialize,
{
    value.serialize(serializer)
}

/// Serializes a Rust type into a TOML string.
///
/// Tables are written as a TOML document (`key = value` lines plus `[table]`
/// and `[[array.of.tables]]` sections); any other root value is written as a
/// bare TOML value.
pub fn to_string<T>(value: &T) -> Result<String, TextError>
where
    T: Serialize,
{
    // 使用自定义序列化器将值转换为 Value
    let serializer = TomlSerializer {};
    let toml_value = value.serialize(serializer)?;

    let mut output = String::new();
    match &toml_value {
        TomlValue::Table(table) => write_table(&mut output, table, &mut Vec::new()),
        other => {
            output.push_str(&format_value(other));
            output.push('\n');
        }
    }

    Ok(output)
}

/// Writes a table's entries as a TOML document body at `path`.
fn write_table(output: &mut String, table: &TomlTable, path: &mut Vec<String>) {
    let mut keys: Vec<&String> = table.dict.keys().collect();
    keys.sort();

    for key in &keys {
        let value = &table.dict[*key];
        if is_table(value) || is_array_of_tables(value) {
            continue;
        }
        output.push_str(&format!("{} = {}\n", format_key(key), format_value(value)));
    }

    for key in &keys {
        let value = &table.dict[*key];
        if let TomlValue::Table(child) = value {
            if !output.is_empty() && !output.ends_with("\n\n") {
                output.push('\n');
            }
            path.push((*key).clone());
            output.push_str(&format!("[{}]\n", format_path(path)));
            write_table(output, child, path);
            path.pop();
        }
    }

    for key in &keys {
        let value = &table.dict[*key];
        if let TomlValue::Array(array) = value {
            if !is_array_of_tables(value) {
                continue;
            }
            path.push((*key).clone());
            for item in &array.list {
                if let TomlValue::Table(child) = item {
                    output.push('\n');
                    output.push_str(&format!("[[{}]]\n", format_path(path)));
                    write_table(output, child, path);
                }
            }
            path.pop();
        }
    }
}

fn is_table(value: &TomlValue) -> bool {
    matches!(value, TomlValue::Table(_))
}

fn is_array_of_tables(value: &TomlValue) -> bool {
    matches!(value, TomlValue::Array(TomlArray { list }) if !list.is_empty() && list.iter().all(|item| matches!(item, TomlValue::Table(_))))
}

fn format_path(path: &[String]) -> String {
    path.iter().map(|segment| format_key(segment)).collect::<Vec<_>>().join(".")
}

fn format_key(key: &str) -> String {
    if !key.is_empty() && key.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
        key.to_string()
    }
    else {
        format!("\"{}\"", escape_string(key))
    }
}

fn format_value(value: &TomlValue) -> String {
    match value {
        TomlValue::String(text) => format!("\"{}\"", escape_string(text)),
        TomlValue::Integer(value) => value.to_string(),
        TomlValue::Float(value) => format_float(*value),
        TomlValue::Boolean(value) => value.to_string(),
        TomlValue::DateTime(value) => value.clone(),
        TomlValue::Array(TomlArray { list }) => {
            let items = list.iter().map(format_value).collect::<Vec<_>>();
            format!("[{}]", items.join(", "))
        }
        TomlValue::Table(TomlTable { dict }) => {
            let mut keys: Vec<&String> = dict.keys().collect();
            keys.sort();
            let items = keys.iter().map(|key| format!("{} = {}", format_key(key), format_value(&dict[*key]))).collect::<Vec<_>>();
            format!("{{{}}}", items.join(", "))
        }
    }
}

fn format_float(value: f64) -> String {
    if value.is_nan() {
        "nan".to_string()
    }
    else if value.is_infinite() {
        if value.is_sign_negative() { "-inf".to_string() } else { "inf".to_string() }
    }
    else if value.fract() == 0.0 {
        format!("{value:.1}")
    }
    else {
        value.to_string()
    }
}

fn escape_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04X}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}

struct TomlSerializer {}

impl Default for TomlSerializer {
    fn default() -> Self {
        Self {}
    }
}

impl Serializer for TomlSerializer {
    type Ok = TomlValue;
    type Error = TextError;

    type SerializeSeq = TomlSeqSerializer;
    type SerializeTuple = TomlSeqSerializer;
    type SerializeTupleStruct = TomlSeqSerializer;
    type SerializeTupleVariant = TomlSeqSerializer;
    type SerializeMap = TomlMapSerializer;
    type SerializeStruct = TomlMapSerializer;
    type SerializeStructVariant = TomlMapSerializer;

    fn serialize_bool(self, v: bool) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Boolean(v))
    }

    fn serialize_i8(self, v: i8) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Integer(v as i64))
    }

    fn serialize_i16(self, v: i16) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Integer(v as i64))
    }

    fn serialize_i32(self, v: i32) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Integer(v as i64))
    }

    fn serialize_i64(self, v: i64) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Integer(v))
    }

    fn serialize_u8(self, v: u8) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Integer(v as i64))
    }

    fn serialize_u16(self, v: u16) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Integer(v as i64))
    }

    fn serialize_u32(self, v: u32) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Integer(v as i64))
    }

    fn serialize_u64(self, v: u64) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Integer(v as i64))
    }

    fn serialize_f32(self, v: f32) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Float(v as f64))
    }

    fn serialize_f64(self, v: f64) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Float(v))
    }

    fn serialize_char(self, v: char) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::String(v.to_string()))
    }

    fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::String(v.to_string()))
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::String(String::from_utf8_lossy(v).to_string()))
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        // TOML doesn't have a null type, so we'll use a string "null"
        Ok(TomlValue::String("null".to_string()))
    }

    fn serialize_some<T>(self, value: &T) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        // TOML doesn't have a unit type, so we'll use a string "null"
        Ok(TomlValue::String("null".to_string()))
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        self.serialize_unit()
    }

    fn serialize_unit_variant(self, _name: &'static str, _variant_index: u32, variant: &'static str) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::String(variant.to_string()))
    }

    fn serialize_newtype_struct<T>(self, _name: &'static str, value: &T) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(self)
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Ok(TomlSeqSerializer { items: Vec::with_capacity(len.unwrap_or(0)) })
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Ok(TomlSeqSerializer { items: Vec::with_capacity(len) })
    }

    fn serialize_tuple_struct(self, _name: &'static str, len: usize) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Ok(TomlSeqSerializer { items: Vec::with_capacity(len) })
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Ok(TomlSeqSerializer { items: Vec::with_capacity(len) })
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Ok(TomlMapSerializer { entries: std::collections::HashMap::with_capacity(len.unwrap_or(0)), current_key: None })
    }

    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<Self::SerializeStruct, Self::Error> {
        Ok(TomlMapSerializer { entries: std::collections::HashMap::with_capacity(len), current_key: None })
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Ok(TomlMapSerializer { entries: std::collections::HashMap::with_capacity(len), current_key: None })
    }
}

struct TomlSeqSerializer {
    items: Vec<TomlValue>,
}

impl serde::ser::SerializeSeq for TomlSeqSerializer {
    type Ok = TomlValue;
    type Error = TextError;

    fn serialize_element<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let serializer = TomlSerializer {};
        let item = value.serialize(serializer)?;
        self.items.push(item);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Array(TomlArray { list: self.items }))
    }
}

impl serde::ser::SerializeTuple for TomlSeqSerializer {
    type Ok = TomlValue;
    type Error = TextError;

    fn serialize_element<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let serializer = TomlSerializer {};
        let item = value.serialize(serializer)?;
        self.items.push(item);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Array(TomlArray { list: self.items }))
    }
}

impl serde::ser::SerializeTupleStruct for TomlSeqSerializer {
    type Ok = TomlValue;
    type Error = TextError;

    fn serialize_field<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let serializer = TomlSerializer {};
        let item = value.serialize(serializer)?;
        self.items.push(item);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Array(TomlArray { list: self.items }))
    }
}

impl serde::ser::SerializeTupleVariant for TomlSeqSerializer {
    type Ok = TomlValue;
    type Error = TextError;

    fn serialize_field<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let serializer = TomlSerializer {};
        let item = value.serialize(serializer)?;
        self.items.push(item);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Array(TomlArray { list: self.items }))
    }
}

struct TomlMapSerializer {
    entries: std::collections::HashMap<String, TomlValue>,
    current_key: Option<String>,
}

impl serde::ser::SerializeMap for TomlMapSerializer {
    type Ok = TomlValue;
    type Error = TextError;

    fn serialize_key<T>(&mut self, key: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        // 使用自定义序列化器将键转换为 TomlValue，再取原始字符串
        let serializer = TomlSerializer {};
        let key_value = key.serialize(serializer)?;
        let key_str = match key_value {
            TomlValue::String(text) => text,
            other => other.to_string(),
        };
        self.current_key = Some(key_str);
        Ok(())
    }

    fn serialize_value<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let serializer = TomlSerializer {};
        let value_value = value.serialize(serializer)?;
        if let Some(key) = self.current_key.take() {
            self.entries.insert(key, value_value);
        }
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Table(TomlTable { dict: self.entries }))
    }
}

impl serde::ser::SerializeStruct for TomlMapSerializer {
    type Ok = TomlValue;
    type Error = TextError;

    fn serialize_field<T>(&mut self, key: &'static str, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let serializer = TomlSerializer {};
        let value_value = value.serialize(serializer)?;
        self.entries.insert(key.to_string(), value_value);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Table(TomlTable { dict: self.entries }))
    }
}

impl serde::ser::SerializeStructVariant for TomlMapSerializer {
    type Ok = TomlValue;
    type Error = TextError;

    fn serialize_field<T>(&mut self, key: &'static str, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let serializer = TomlSerializer {};
        let value_value = value.serialize(serializer)?;
        self.entries.insert(key.to_string(), value_value);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(TomlValue::Table(TomlTable { dict: self.entries }))
    }
}
