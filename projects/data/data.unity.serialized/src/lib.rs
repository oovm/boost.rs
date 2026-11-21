//! Unity SerializedFile 读取器。
//!
//! 覆盖 Unity 2017.4 至 2022 的 SerializedFile 头部、类型表、对象记录、外部引用与
//! TypeTree。对象值以 `serde_json::Value` 形式给出，供 OpenGRE 组织导出报告。
//! 文件被剥离 TypeTree 时，从 crate 内嵌的类型库按 Unity 版本与 ClassID 回退查找。
//!
//! 格式解析、TypeTree 匹配与 JSON 解码都在 crate 内实现，不依赖外部 Unity 资源库。

#![warn(missing_docs)]

use std::{borrow::Cow, collections::HashMap, fmt};

use serde_json::Value;

mod class_id;
mod lz4;
mod reader;
mod tpk;
mod typetree;
mod value;
mod version;

use reader::{ByteReader, read_count};
use typetree::TypeTree;
use version::parse_unity_version;

/// SerializedFile 读取失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnityError {
    /// 字节不足或字段越界。
    Parse(String),
    /// 版本或特性不在本 crate 的声明范围内。
    Unsupported(String),
}

impl fmt::Display for UnityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) | Self::Unsupported(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for UnityError {}

/// Unity 版本号，例如 `2020.3.41f1`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnityVersion {
    /// 主版本。
    pub major: u32,
    /// 次版本。
    pub minor: u32,
    /// 修订号。
    pub build: u32,
    /// 发布类型（`a` / `b` / `f` / `p` / `x`）。
    pub kind: char,
    /// 发布序号。
    pub revision: u32,
    /// 发布序号之后的额外后缀，例如中国版 `c1`。
    pub suffix: String,
    /// 原始文本。
    pub text: String,
}

impl fmt::Display for UnityVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

/// 一条对象记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectInfo {
    /// `m_PathID`。
    pub path_id: i64,
    /// 对象数据相对文件起点的偏移。
    pub offset: i64,
    /// 对象字节数。
    pub size: u32,
    /// Unity ClassID。
    pub class_id: i32,
}

/// 一条外部引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct External {
    /// 外部文件名。
    pub path: String,
    /// 外部资源路径。
    pub path_name: String,
}

/// 文件头保存的端序。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Endianness {
    #[default]
    Little,
    Big,
}

impl Endianness {
    fn is_little(self) -> bool {
        matches!(self, Self::Little)
    }
}

/// 类型表条目：ClassID 与可选的内嵌 TypeTree。
#[derive(Debug, Clone, Default)]
struct SerializedType {
    class_id: i32,
    typetree: Option<TypeTree>,
}

/// 解析后的 SerializedFile。
#[derive(Debug, Clone, Default)]
pub struct SerializedFile {
    objects: Vec<ObjectInfo>,
    externals: Vec<External>,
    unity_version: Option<UnityVersion>,
    enable_type_tree: bool,
    type_count: usize,
    types: Vec<SerializedType>,
    object_types: HashMap<i64, usize>,
    endianness: Endianness,
}

impl SerializedFile {
    /// 解析完整文件字节。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, UnityError> {
        let mut reader = ByteReader::new(bytes);
        let header = read_header(&mut reader)?;
        reader.set_little(header.endianness.is_little());

        let version = header.version;
        let mut unity_version = None;
        if version >= 9 {
            let text = reader.read_cstr()?;
            if text != "0.0.0" {
                unity_version = Some(parse_unity_version(&text)?);
            }
        }

        if version >= 10 {
            reader.read_i32()?; // target platform
        }
        let enable_type_tree = if version >= 11 { reader.read_u8()? != 0 } else { false };

        let type_count = read_count(&mut reader, 4, "type count")?;
        let mut types = Vec::with_capacity(type_count.min(4096));
        for _ in 0..type_count {
            types.push(read_serialized_type(&mut reader, version, enable_type_tree, false)?);
        }

        let big_id = if (7..14).contains(&version) { Some(reader.read_i32()?) } else { None };

        let object_count = read_count(&mut reader, 8, "object count")?;
        let mut objects = Vec::with_capacity(object_count.min(65536));
        let mut object_types = HashMap::with_capacity(object_count.min(65536));
        for _ in 0..object_count {
            let (object, type_index) = read_object(&mut reader, &header, big_id, &types)?;
            object_types.entry(object.path_id).or_insert(type_index);
            objects.push(object);
        }

        if version >= 11 {
            let script_count = read_count(&mut reader, 8, "script type count")?;
            for _ in 0..script_count {
                reader.read_i32()?;
                if version < 14 {
                    reader.read_i32()?;
                }
                else {
                    reader.align(4)?;
                    reader.read_i64()?;
                }
            }
        }

        let external_count = read_count(&mut reader, 1, "external count")?;
        let mut externals = Vec::with_capacity(external_count.min(65536));
        for _ in 0..external_count {
            externals.push(read_external(&mut reader, version)?);
        }

        if version >= 20 {
            let ref_type_count = read_count(&mut reader, 4, "ref type count")?;
            for _ in 0..ref_type_count {
                read_serialized_type(&mut reader, version, enable_type_tree, true)?;
            }
        }

        if version >= 5 {
            reader.read_cstr()?; // user information
        }

        Ok(Self {
            objects,
            externals,
            unity_version,
            enable_type_tree,
            type_count,
            types,
            object_types,
            endianness: header.endianness,
        })
    }

    /// 全部对象记录。
    pub fn objects(&self) -> &[ObjectInfo] {
        &self.objects
    }

    /// 全部外部引用。
    pub fn externals(&self) -> &[External] {
        &self.externals
    }

    /// 文件头记录的 Unity 版本。
    pub fn unity_version(&self) -> Option<&UnityVersion> {
        self.unity_version.as_ref()
    }

    /// 文件是否内嵌 TypeTree。
    pub fn enable_type_tree(&self) -> bool {
        self.enable_type_tree
    }

    /// 类型表条目数。
    pub fn types_len(&self) -> usize {
        self.type_count
    }

    /// 复制对象原始字节；范围越界时返回错误。
    pub fn read_raw<'a>(&self, object: &ObjectInfo, bytes: &'a [u8]) -> Result<&'a [u8], UnityError> {
        let start = usize::try_from(object.offset).map_err(|_| UnityError::Parse(format!("negative object offset {}", object.offset)))?;
        let end = start.checked_add(object.size as usize).ok_or_else(|| UnityError::Parse("object byte range overflow".into()))?;
        bytes.get(start..end).ok_or_else(|| UnityError::Parse(format!("object range {start}..{end} exceeds file length {}", bytes.len())))
    }

    /// 按 TypeTree 把对象解码为 JSON；缺少可用 TypeTree 时返回错误。
    pub fn read_value(&self, object: &ObjectInfo, bytes: &[u8]) -> Result<Value, UnityError> {
        let raw = self.read_raw(object, bytes)?;
        let typetree = self.typetree_for(object)?;
        value::decode_value(&typetree, raw, self.endianness.is_little())
    }

    fn typetree_for(&self, object: &ObjectInfo) -> Result<Cow<'_, TypeTree>, UnityError> {
        if let Some(index) = self.type_index_for(object) {
            if let Some(typetree) = self.types.get(index).and_then(|serialized| serialized.typetree.as_ref()) {
                return Ok(Cow::Borrowed(typetree));
            }
        }

        let unity_version = self.unity_version.as_ref().ok_or_else(|| {
            UnityError::Unsupported(format!(
                "no embedded typetree for class {} and file has no unity version",
                class_id::class_name_for(object.class_id)
            ))
        })?;
        let typetree = tpk::lookup_typetree(object.class_id, unity_version).ok_or_else(|| {
            UnityError::Unsupported(format!(
                "no typetree for class {} ({}) in unity {}",
                object.class_id,
                class_id::class_name_for(object.class_id),
                unity_version
            ))
        })?;
        Ok(Cow::Owned(typetree))
    }

    fn type_index_for(&self, object: &ObjectInfo) -> Option<usize> {
        self.object_types.get(&object.path_id).copied()
    }
}

#[derive(Debug, Clone, Copy)]
struct Header {
    version: u32,
    data_offset: i64,
    endianness: Endianness,
}

fn read_header(reader: &mut ByteReader<'_>) -> Result<Header, UnityError> {
    let metadata_size = reader.read_u32_be()?;
    let file_size = reader.read_u32_be()? as i64;
    let version = reader.read_u32_be()?;
    let data_offset = reader.read_u32_be()? as i64;

    let endianness_byte = if version >= 9 {
        let byte = reader.read_u8()?;
        reader.read_bytes(3)?;
        byte
    }
    else {
        let delta = file_size - metadata_size as i64;
        let position = 16i64
            .checked_add(delta)
            .filter(|position| *position >= 0)
            .ok_or_else(|| UnityError::Parse("invalid pre-v9 header layout".into()))?;
        reader.seek(position as usize)?;
        reader.read_u8()?
    };
    let endianness = match endianness_byte {
        0 => Endianness::Little,
        1 => Endianness::Big,
        other => {
            return Err(UnityError::Parse(format!("invalid endianness byte {other}")));
        }
    };

    let mut data_offset = data_offset;
    if version >= 22 {
        let _metadata_size = reader.read_u32_be()?;
        let _file_size = reader.read_i64_be()?;
        data_offset = reader.read_i64_be()?;
        let _unknown = reader.read_i64_be()?;
    }
    if data_offset < 0 {
        return Err(UnityError::Parse(format!("negative data offset {data_offset}")));
    }

    Ok(Header { version, data_offset, endianness })
}

fn read_serialized_type(
    reader: &mut ByteReader<'_>,
    version: u32,
    enable_type_tree: bool,
    is_ref_type: bool,
) -> Result<SerializedType, UnityError> {
    let class_id = reader.read_i32()?;

    if version >= 16 {
        reader.read_u8()?; // is stripped
    }
    let script_type_index = if version >= 17 { Some(reader.read_i16()?) } else { None };

    if version >= 13 {
        let has_script_id =
            (is_ref_type && script_type_index.unwrap_or(-1) >= 0) || (version < 16 && class_id < 0) || (version >= 16 && class_id == 114);
        if has_script_id {
            reader.read_bytes(16)?;
        }
        reader.read_bytes(16)?; // old type hash
    }

    let mut typetree = None;
    if enable_type_tree {
        typetree = Some(if version >= 12 || version == 10 {
            typetree::parse_blob(reader, version)?
        }
        else {
            typetree::parse_node_sequence(reader, version)?
        });

        if version >= 21 {
            if is_ref_type {
                reader.read_cstr()?;
                reader.read_cstr()?;
                reader.read_cstr()?;
            }
            else {
                let dependency_count = read_count(reader, 4, "type dependency count")?;
                for _ in 0..dependency_count {
                    reader.read_i32()?;
                }
            }
        }
    }

    Ok(SerializedType { class_id, typetree })
}

fn read_object(
    reader: &mut ByteReader<'_>,
    header: &Header,
    big_id: Option<i32>,
    types: &[SerializedType],
) -> Result<(ObjectInfo, usize), UnityError> {
    let version = header.version;
    let path_id = if big_id.is_some_and(|enabled| enabled > 0) {
        reader.read_i64()?
    }
    else if version < 14 {
        reader.read_i32()? as i64
    }
    else {
        reader.align(4)?;
        reader.read_i64()?
    };

    let byte_start = if version >= 22 { reader.read_i64()? } else { reader.read_u32()? as i64 };
    let offset = byte_start.checked_add(header.data_offset).ok_or_else(|| UnityError::Parse("object offset overflow".into()))?;

    let size = reader.read_u32()?;
    let type_id = reader.read_i32()?;
    let type_index = usize::try_from(type_id).unwrap_or(usize::MAX);
    let class_id = if version < 16 {
        reader.read_u16()? as i32
    }
    else {
        types.get(type_index).ok_or_else(|| UnityError::Parse(format!("object references undefined type {type_id}")))?.class_id
    };

    if version < 11 {
        reader.read_u16()?; // is destroyed
    }
    else if version < 17 {
        reader.read_i16()?; // script type index
    }
    if version == 15 || version == 16 {
        reader.read_u8()?; // stripped
    }

    Ok((ObjectInfo { path_id, offset, size, class_id }, type_index))
}

fn read_external(reader: &mut ByteReader<'_>, version: u32) -> Result<External, UnityError> {
    let mut path = String::new();
    if version >= 6 {
        path = reader.read_cstr()?;
    }
    if version >= 5 {
        reader.read_bytes(16)?; // guid
        reader.read_i32()?; // type
    }
    let path_name = reader.read_cstr()?;
    Ok(External { path, path_name })
}

/// Unity ClassID 的稳定可读名称，未知编号回退为 `UnknownType`。
pub fn class_name(class_id: i32) -> &'static str {
    class_id::class_name_for(class_id)
}

/// Unity ClassID 的稳定可读名称；未知编号返回 `None`。
pub fn class_name_opt(class_id: i32) -> Option<&'static str> {
    class_id::class_name_opt(class_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE_SIZE: usize = 32;

    fn align_vec(bytes: &mut Vec<u8>, align: usize) {
        while bytes.len() % align != 0 {
            bytes.push(0);
        }
    }

    fn push_cstr(bytes: &mut Vec<u8>, value: &str) {
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
    }

    fn make_typetree_blob() -> Vec<u8> {
        let strings = b"GameObject\0Base\0string\0m_Name\0";
        let mut nodes = Vec::new();
        for (level, type_offset, name_offset, meta_flag) in [(0u8, 0u32, 11u32, 0i32), (1, 16, 23, 0x4000)] {
            nodes.extend_from_slice(&1u16.to_le_bytes());
            nodes.push(level);
            nodes.push(0);
            nodes.extend_from_slice(&type_offset.to_le_bytes());
            nodes.extend_from_slice(&name_offset.to_le_bytes());
            nodes.extend_from_slice(&(-1i32).to_le_bytes());
            nodes.extend_from_slice(&(nodes.len() as i32 / NODE_SIZE as i32).to_le_bytes());
            nodes.extend_from_slice(&meta_flag.to_le_bytes());
            nodes.extend_from_slice(&0u64.to_le_bytes());
        }

        let mut blob = Vec::new();
        blob.extend_from_slice(&2i32.to_le_bytes());
        blob.extend_from_slice(&(strings.len() as i32).to_le_bytes());
        blob.extend_from_slice(&nodes);
        blob.extend_from_slice(strings);
        blob
    }

    fn make_serialized_file(version: u32) -> Vec<u8> {
        let mut metadata = Vec::new();
        push_cstr(&mut metadata, "2020.3.41f1");
        metadata.extend_from_slice(&0i32.to_le_bytes()); // target platform
        metadata.push(1); // enable type tree

        metadata.extend_from_slice(&1i32.to_le_bytes()); // type count
        metadata.extend_from_slice(&1i32.to_le_bytes()); // class id: GameObject
        metadata.push(0); // stripped
        metadata.extend_from_slice(&(-1i16).to_le_bytes()); // script type index
        metadata.extend_from_slice(&[0u8; 16]); // old type hash
        metadata.extend_from_slice(&make_typetree_blob());
        metadata.extend_from_slice(&0i32.to_le_bytes()); // type dependencies

        let object_data = {
            let mut data = Vec::new();
            data.extend_from_slice(&8u32.to_le_bytes());
            data.extend_from_slice(b"TestName");
            align_vec(&mut data, 4);
            data
        };

        metadata.extend_from_slice(&1i32.to_le_bytes()); // object count
        align_vec(&mut metadata, 4);
        metadata.extend_from_slice(&100i64.to_le_bytes()); // path id
        if version >= 22 {
            metadata.extend_from_slice(&0i64.to_le_bytes()); // byte start
        }
        else {
            metadata.extend_from_slice(&0u32.to_le_bytes());
        }
        metadata.extend_from_slice(&(object_data.len() as u32).to_le_bytes());
        metadata.extend_from_slice(&0i32.to_le_bytes()); // type id

        metadata.extend_from_slice(&0i32.to_le_bytes()); // script types
        metadata.extend_from_slice(&0i32.to_le_bytes()); // externals
        if version >= 20 {
            metadata.extend_from_slice(&0i32.to_le_bytes()); // ref types
        }
        push_cstr(&mut metadata, ""); // user information

        let header_size = if version >= 22 { 48 } else { 20 };
        let data_offset = (header_size + metadata.len() + 7) & !7;
        let file_size = data_offset + object_data.len();

        let mut file = Vec::new();
        if version >= 22 {
            file.extend_from_slice(&0u32.to_be_bytes());
            file.extend_from_slice(&0u32.to_be_bytes());
            file.extend_from_slice(&version.to_be_bytes());
            file.extend_from_slice(&0u32.to_be_bytes());
            file.push(0); // little endian
            file.extend_from_slice(&[0, 0, 0]);
            file.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
            file.extend_from_slice(&(file_size as i64).to_be_bytes());
            file.extend_from_slice(&(data_offset as i64).to_be_bytes());
            file.extend_from_slice(&0i64.to_be_bytes());
        }
        else {
            file.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
            file.extend_from_slice(&(file_size as u32).to_be_bytes());
            file.extend_from_slice(&version.to_be_bytes());
            file.extend_from_slice(&(data_offset as u32).to_be_bytes());
            file.push(0);
            file.extend_from_slice(&[0, 0, 0]);
        }
        assert_eq!(file.len(), header_size);
        file.extend_from_slice(&metadata);
        while file.len() < data_offset {
            file.push(0);
        }
        file.extend_from_slice(&object_data);
        assert_eq!(file.len(), file_size);
        file
    }

    #[test]
    fn parses_v22_file_and_object_fields() {
        let bytes = make_serialized_file(22);
        let file = SerializedFile::from_bytes(&bytes).unwrap();
        assert_eq!(file.unity_version().unwrap().to_string(), "2020.3.41f1");
        assert!(file.enable_type_tree());
        assert_eq!(file.types_len(), 1);
        assert_eq!(file.objects().len(), 1);

        let object = &file.objects()[0];
        assert_eq!(object.path_id, 100);
        assert_eq!(object.size, 12);
        assert_eq!(object.class_id, 1);
        assert_eq!(object.offset as usize, {
            let header = read_header(&mut ByteReader::new(&bytes)).unwrap();
            header.data_offset as usize
        });
        assert_eq!(file.read_raw(object, &bytes).unwrap(), b"\x08\0\0\0TestName");
        assert_eq!(file.read_value(object, &bytes).unwrap(), serde_json::json!({"m_Name": "TestName"}));
        assert_eq!(class_name(object.class_id), "GameObject");
    }

    #[test]
    fn parses_v21_file() {
        let bytes = make_serialized_file(21);
        let file = SerializedFile::from_bytes(&bytes).unwrap();
        let object = &file.objects()[0];
        assert_eq!(object.path_id, 100);
        assert_eq!(object.class_id, 1);
        assert_eq!(file.read_value(object, &bytes).unwrap(), serde_json::json!({"m_Name": "TestName"}));
    }

    #[test]
    fn truncated_input_is_rejected() {
        let bytes = make_serialized_file(22);
        for len in [0, 4, 20, 47, 48, 60] {
            assert!(SerializedFile::from_bytes(&bytes[..len]).is_err());
        }
    }

    #[test]
    fn negative_counts_are_rejected() {
        let mut bytes = make_serialized_file(22);
        let type_count_position = 48 + "2020.3.41f1\0".len() + 4 + 1;
        bytes[type_count_position..type_count_position + 4].copy_from_slice(&(-1i32).to_le_bytes());
        assert!(SerializedFile::from_bytes(&bytes).is_err());
    }

    #[test]
    fn oversized_object_range_is_rejected() {
        let bytes = make_serialized_file(22);
        let file = SerializedFile::from_bytes(&bytes).unwrap();
        let mut object = file.objects()[0];
        object.size = u32::MAX;
        assert!(file.read_raw(&object, &bytes).is_err());
        object.size = 12;
        object.offset = bytes.len() as i64 - 1;
        assert!(file.read_raw(&object, &bytes).is_err());
    }

    #[test]
    fn tpk_fallback_without_embedded_typetree() {
        let mut metadata = Vec::new();
        push_cstr(&mut metadata, "2020.3.41f1");
        metadata.extend_from_slice(&0i32.to_le_bytes());
        metadata.push(0); // no embedded type tree
        metadata.extend_from_slice(&1i32.to_le_bytes()); // one type
        metadata.extend_from_slice(&4i32.to_le_bytes()); // class id: Transform
        metadata.push(0);
        metadata.extend_from_slice(&(-1i16).to_le_bytes());
        metadata.extend_from_slice(&[0u8; 16]);
        metadata.extend_from_slice(&1i32.to_le_bytes()); // object count
        align_vec(&mut metadata, 4);
        metadata.extend_from_slice(&7i64.to_le_bytes()); // path id
        metadata.extend_from_slice(&0i64.to_le_bytes()); // byte start
        metadata.extend_from_slice(&0u32.to_le_bytes()); // size
        metadata.extend_from_slice(&0i32.to_le_bytes()); // type id
        metadata.extend_from_slice(&0i32.to_le_bytes()); // script types
        metadata.extend_from_slice(&0i32.to_le_bytes()); // externals
        metadata.extend_from_slice(&0i32.to_le_bytes()); // ref types
        push_cstr(&mut metadata, "");

        let header_size = 48;
        let data_offset = (header_size + metadata.len() + 7) & !7;
        let mut file = Vec::new();
        file.extend_from_slice(&0u32.to_be_bytes());
        file.extend_from_slice(&0u32.to_be_bytes());
        file.extend_from_slice(&22u32.to_be_bytes());
        file.extend_from_slice(&0u32.to_be_bytes());
        file.push(0);
        file.extend_from_slice(&[0, 0, 0]);
        file.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
        file.extend_from_slice(&(data_offset as i64).to_be_bytes());
        file.extend_from_slice(&(data_offset as i64).to_be_bytes());
        file.extend_from_slice(&0i64.to_be_bytes());
        file.extend_from_slice(&metadata);
        while file.len() < data_offset {
            file.push(0);
        }

        let parsed = SerializedFile::from_bytes(&file).unwrap();
        assert!(!parsed.enable_type_tree());
        assert_eq!(parsed.types_len(), 1);
        let object = &parsed.objects()[0];
        assert_eq!(object.class_id, 4);
        let typetree = parsed.typetree_for(object).unwrap();
        assert_eq!(typetree.m_type, "Transform");
        assert!(!typetree.children.is_empty());

        let bytes = make_serialized_file(22);
        let missing = SerializedFile::from_bytes(&bytes).unwrap();
        let mut unknown = missing.objects()[0];
        unknown.path_id = 999;
        unknown.class_id = 424_242;
        assert!(matches!(missing.read_value(&unknown, &bytes), Err(UnityError::Unsupported(_))));
    }
}
