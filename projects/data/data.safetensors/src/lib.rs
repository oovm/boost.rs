//! Safetensors 容器编解码。
//!
//! 只负责字节容器与头部元数据，不理解张量语义、设备或自动微分。DL 模型通过
//! `dl-model` 把 `F32` 张量映射到这里，因此容器格式的唯一实现留在 `data.*`。
//!
//! 支持读取标准 `F32` 等常见 dtype 的负载；解码不依赖第三方 JSON 库。

#![warn(missing_docs)]

use std::{collections::BTreeMap, fmt};

/// 编码输入：借用形态的张量条目。
#[derive(Clone, Copy, Debug)]
pub struct TensorView<'a> {
    /// dtype 名称，例如 `F32`。
    pub dtype: &'a str,
    /// 形状。
    pub shape: &'a [usize],
    /// 原始小端字节。
    pub bytes: &'a [u8],
}

/// 解码输出：拥有数据的张量条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedTensor {
    /// dtype 名称。
    pub dtype: String,
    /// 形状。
    pub shape: Vec<usize>,
    /// 原始小端字节。
    pub bytes: Vec<u8>,
}

/// 容器格式错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatError {
    detail: String,
}

impl FormatError {
    fn new(detail: impl Into<String>) -> Self {
        Self { detail: detail.into() }
    }
}

impl fmt::Display for FormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for FormatError {}

/// 按名称排序编码为 safetensors 字节。
pub fn encode(tensors: &BTreeMap<String, TensorView<'_>>) -> Result<Vec<u8>, FormatError> {
    let mut header = String::from("{");
    let mut offset = 0usize;
    for (position, (name, tensor)) in tensors.iter().enumerate() {
        if tensor.dtype.is_empty() {
            return Err(FormatError::new(format!("tensor {name:?} has empty dtype")));
        }
        if position > 0 {
            header.push(',');
        }
        header.push('"');
        escape_into(&mut header, name);
        header.push_str("\":{\"dtype\":\"");
        escape_into(&mut header, tensor.dtype);
        header.push_str("\",\"shape\":[");
        for (axis, dimension) in tensor.shape.iter().enumerate() {
            if axis > 0 {
                header.push(',');
            }
            header.push_str(&dimension.to_string());
        }
        header.push_str("],\"data_offsets\":[");
        header.push_str(&offset.to_string());
        header.push(',');
        header.push_str(&(offset + tensor.bytes.len()).to_string());
        header.push_str("]}");
        offset += tensor.bytes.len();
    }
    header.push('}');

    let mut header_bytes = header.into_bytes();
    while header_bytes.len() % 8 != 0 {
        header_bytes.push(b' ');
    }
    let mut output = Vec::with_capacity(8 + header_bytes.len() + offset);
    output.extend_from_slice(&(header_bytes.len() as u64).to_le_bytes());
    output.extend_from_slice(&header_bytes);
    for tensor in tensors.values() {
        output.extend_from_slice(tensor.bytes);
    }
    Ok(output)
}

/// 解码 safetensors 字节；`__metadata__` 会被忽略。
pub fn decode(data: &[u8]) -> Result<BTreeMap<String, OwnedTensor>, FormatError> {
    if data.len() < 8 {
        return Err(FormatError::new("file shorter than 8-byte header length"));
    }
    let header_len = u64::from_le_bytes(data[..8].try_into().expect("8 bytes")) as usize;
    let payload_start = 8usize.checked_add(header_len).ok_or_else(|| FormatError::new("header length overflow"))?;
    if payload_start > data.len() {
        return Err(FormatError::new("header length exceeds file size"));
    }
    let header = std::str::from_utf8(&data[8..payload_start]).map_err(|error| FormatError::new(format!("header is not UTF-8: {error}")))?;
    let payload = &data[payload_start..];
    let parsed = Parser::new(header).parse()?;
    let entries = parsed.as_object().ok_or_else(|| FormatError::new("header is not a JSON object"))?;

    let mut output = BTreeMap::new();
    for (name, value) in entries {
        if name == "__metadata__" {
            continue;
        }
        let fields = value.as_object().ok_or_else(|| FormatError::new(format!("tensor {name:?} entry is not an object")))?;
        let dtype =
            field(fields, "dtype")?.as_string().ok_or_else(|| FormatError::new(format!("tensor {name:?} dtype is not a string")))?.to_owned();
        let shape = field(fields, "shape")?
            .as_array()
            .ok_or_else(|| FormatError::new(format!("tensor {name:?} shape is not an array")))?
            .iter()
            .map(|dimension| {
                dimension
                    .as_number()
                    .map(|number| number as usize)
                    .ok_or_else(|| FormatError::new(format!("tensor {name:?} shape is not unsigned")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let offsets = field(fields, "data_offsets")?
            .as_array()
            .ok_or_else(|| FormatError::new(format!("tensor {name:?} data_offsets is not an array")))?;
        if offsets.len() != 2 {
            return Err(FormatError::new(format!("tensor {name:?} needs two data offsets")));
        }
        let start = offsets[0].as_number().ok_or_else(|| FormatError::new(format!("tensor {name:?} start offset is not unsigned")))? as usize;
        let end = offsets[1].as_number().ok_or_else(|| FormatError::new(format!("tensor {name:?} end offset is not unsigned")))? as usize;
        if start > end || end > payload.len() {
            return Err(FormatError::new(format!("tensor {name:?} offsets out of payload")));
        }
        let expected = element_size(&dtype)?;
        let count = shape.iter().product::<usize>();
        if (end - start) != count * expected {
            return Err(FormatError::new(format!("tensor {name:?} payload {} bytes does not match {count} x {expected}", end - start)));
        }
        output.insert(name.to_string(), OwnedTensor { dtype, shape, bytes: payload[start..end].to_vec() });
    }
    Ok(output)
}

fn element_size(dtype: &str) -> Result<usize, FormatError> {
    match dtype {
        "F64" | "I64" | "U64" => Ok(8),
        "F32" | "I32" | "U32" => Ok(4),
        "F16" | "BF16" | "I16" | "U16" => Ok(2),
        "I8" | "U8" | "BOOL" => Ok(1),
        other => Err(FormatError::new(format!("unsupported dtype {other:?}"))),
    }
}

fn field<'a>(fields: &'a [(String, Json)], name: &str) -> Result<&'a Json, FormatError> {
    fields.iter().find(|(key, _)| key == name).map(|(_, value)| value).ok_or_else(|| FormatError::new(format!("missing field {name:?}")))
}

fn escape_into(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                output.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => output.push(character),
        }
    }
}

#[derive(Clone, Debug)]
enum Json {
    Object(Vec<(String, Json)>),
    Array(Vec<Json>),
    String(String),
    Number(u64),
    Bool(bool),
    Null,
}

impl Json {
    fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Self::Object(fields) => Some(fields),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Json]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    fn as_string(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    fn as_number(&self) -> Option<u64> {
        match self {
            Self::Number(value) => Some(*value),
            _ => None,
        }
    }
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, bytes: text.as_bytes(), position: 0 }
    }

    fn parse(mut self) -> Result<Json, FormatError> {
        self.skip_whitespace();
        let value = self.value()?;
        self.skip_whitespace();
        if self.position != self.bytes.len() {
            return Err(FormatError::new("trailing bytes after header JSON"));
        }
        Ok(value)
    }

    fn value(&mut self) -> Result<Json, FormatError> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::String(self.string()?)),
            Some(b't') => {
                self.literal("true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(Json::Bool(false))
            }
            Some(b'n') => {
                self.literal("null")?;
                Ok(Json::Null)
            }
            Some(b'0'..=b'9') => Ok(Json::Number(self.number()?)),
            _ => Err(FormatError::new(format!("unexpected byte at {}", self.position))),
        }
    }

    fn object(&mut self) -> Result<Json, FormatError> {
        self.expect(b'{')?;
        let mut fields = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.position += 1;
            return Ok(Json::Object(fields));
        }
        loop {
            self.skip_whitespace();
            let key = self.string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            self.skip_whitespace();
            let value = self.value()?;
            fields.push((key, value));
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b'}') => {
                    self.position += 1;
                    break;
                }
                _ => return Err(FormatError::new("expected ',' or '}' in object")),
            }
        }
        Ok(Json::Object(fields))
    }

    fn array(&mut self) -> Result<Json, FormatError> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.position += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.value()?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b']') => {
                    self.position += 1;
                    break;
                }
                _ => return Err(FormatError::new("expected ',' or ']' in array")),
            }
        }
        Ok(Json::Array(items))
    }

    fn string(&mut self) -> Result<String, FormatError> {
        self.expect(b'"')?;
        let mut output = String::new();
        let mut start = self.position;
        loop {
            match self.bytes.get(self.position) {
                None => return Err(FormatError::new("unterminated string")),
                Some(b'"') => {
                    output.push_str(&self.text[start..self.position]);
                    self.position += 1;
                    return Ok(output);
                }
                Some(b'\\') => {
                    output.push_str(&self.text[start..self.position]);
                    self.position += 1;
                    let escape = *self.bytes.get(self.position).ok_or_else(|| FormatError::new("unterminated escape"))?;
                    self.position += 1;
                    match escape {
                        b'"' => output.push('"'),
                        b'\\' => output.push('\\'),
                        b'/' => output.push('/'),
                        b'b' => output.push('\u{0008}'),
                        b'f' => output.push('\u{000c}'),
                        b'n' => output.push('\n'),
                        b'r' => output.push('\r'),
                        b't' => output.push('\t'),
                        b'u' => {
                            let end = self.position + 4;
                            let digits = self.text.get(self.position..end).ok_or_else(|| FormatError::new("truncated unicode escape"))?;
                            let code = u32::from_str_radix(digits, 16).map_err(|_| FormatError::new("invalid unicode escape"))?;
                            output.push(char::from_u32(code).ok_or_else(|| FormatError::new("invalid unicode scalar"))?);
                            self.position = end;
                        }
                        other => {
                            return Err(FormatError::new(format!("unsupported escape \\{}", other as char)));
                        }
                    }
                    start = self.position;
                }
                Some(_) => self.position += 1,
            }
        }
    }

    fn number(&mut self) -> Result<u64, FormatError> {
        let start = self.position;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.position += 1;
        }
        self.text[start..self.position].parse::<u64>().map_err(|_| FormatError::new("invalid unsigned number"))
    }

    fn literal(&mut self, literal: &str) -> Result<(), FormatError> {
        if self.text[self.position..].starts_with(literal) {
            self.position += literal.len();
            Ok(())
        }
        else {
            Err(FormatError::new(format!("expected {literal}")))
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), FormatError> {
        if self.peek() == Some(byte) {
            self.position += 1;
            Ok(())
        }
        else {
            Err(FormatError::new(format!("expected '{}'", byte as char)))
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_tensors() {
        let left_bytes = [1.0f32, 2.0, 3.0, 4.0].map(f32::to_le_bytes).concat();
        let right_bytes = [5.0f32, 6.0].map(f32::to_le_bytes).concat();
        let mut input = BTreeMap::new();
        input.insert("weight".to_owned(), TensorView { dtype: "F32", shape: &[2, 2], bytes: &left_bytes });
        input.insert("bias".to_owned(), TensorView { dtype: "F32", shape: &[2], bytes: &right_bytes });
        let encoded = encode(&input).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded["weight"].shape, vec![2, 2]);
        assert_eq!(decoded["weight"].bytes, left_bytes.to_vec());
        assert_eq!(decoded["bias"].bytes, right_bytes.to_vec());
    }

    #[test]
    fn rejects_truncated_payload() {
        let mut input = BTreeMap::new();
        let bytes = [1.0f32].map(f32::to_le_bytes).concat();
        input.insert("x".to_owned(), TensorView { dtype: "F32", shape: &[1], bytes: &bytes });
        let mut encoded = encode(&input).unwrap();
        encoded.pop();
        assert!(decode(&encoded).is_err());
    }
}
