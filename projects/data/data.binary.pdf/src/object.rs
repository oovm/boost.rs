use std::collections::BTreeMap;

/// A PDF dictionary keyed by its decoded name (without the leading slash).
pub type Dict = BTreeMap<String, PdfObject>;

/// One PDF object as it appears in the file body.
#[derive(Debug, Clone, PartialEq)]
pub enum PdfObject {
    Null,
    Boolean(bool),
    Integer(i64),
    Real(f64),
    String(Vec<u8>),
    Name(String),
    Array(Vec<PdfObject>),
    Dictionary(Dict),
    Reference(u32, u16),
    Stream(PdfStream),
}

/// A stream object: its dictionary plus the raw, undecoded payload.
#[derive(Debug, Clone, PartialEq)]
pub struct PdfStream {
    pub dict: Dict,
    pub data: Vec<u8>,
}

impl PdfObject {
    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Self::Dictionary(dict) => Some(dict),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[PdfObject]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_name(&self) -> Option<&str> {
        match self {
            Self::Name(name) => Some(name),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }
}

/// Encodes text for a PDF text string.
///
/// ASCII stays readable; anything else becomes UTF-16BE with a byte-order mark,
/// which every conforming reader understands.
pub fn encode_text(text: &str) -> Vec<u8> {
    if text.is_ascii() {
        return text.as_bytes().to_vec();
    }
    let mut bytes = vec![0xFE, 0xFF];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    bytes
}
