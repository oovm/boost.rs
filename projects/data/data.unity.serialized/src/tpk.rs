//! TPK 类型库容器解析与查询。
//!
//! TPK 是 AssetRipper/Tpk 使用的压缩类型库容器：外层是 `TPK*` 头与压缩块，
//! 内层保存 Unity 版本列表、ClassID 到节点根索引的映射、节点缓冲区、字符串
//! 缓冲区与 common string 表。本模块同时提供 LZ4 解压入口与按版本查询
//! TypeTree 的能力。

use std::{collections::HashMap, sync::OnceLock};

use crate::{
    UnityError, UnityVersion,
    lz4::decompress_block,
    reader::{ByteReader, read_count},
    typetree::{TypeTree, build_tree_from_levels},
    version::{unity_version_from_parts, version_at_least},
};

const EMBEDDED_TPK: &[u8] = include_bytes!("../resources/typetree.tpk");

const TPK_MAGIC: &[u8; 4] = b"TPK*";
const TPK_VERSION: u8 = 1;
const COMPRESSION_NONE: u8 = 0;
const COMPRESSION_LZ4: u8 = 1;
const DATA_TYPE_TYPETREE: u8 = 0;

/// 解压并返回 TPK 的负载字节。
pub(crate) fn decompress_container(bytes: &[u8]) -> Result<Vec<u8>, UnityError> {
    let container = TpkContainer::parse(bytes)?;
    match container.compression {
        COMPRESSION_NONE => Ok(container.payload.to_vec()),
        COMPRESSION_LZ4 => decompress_block(container.payload, container.uncompressed_size),
        other => Err(UnityError::Unsupported(format!("unsupported TPK compression method {other}"))),
    }
}

/// 按 Unity 版本与 ClassID 查询 TypeTree。
pub(crate) fn lookup_typetree(class_id: i32, target: &UnityVersion) -> Option<TypeTree> {
    let tpk = embedded_tpk()?;
    let classes = tpk.classes.get(&class_id)?;
    let class = classes.iter().rev().find_map(|(version, class)| version_at_least(target, version).then_some(class))?.as_ref()?;
    let root = class.release_root_node?;

    let mut flat: Vec<(u8, TypeTree)> = Vec::new();
    let mut stack: Vec<(u16, u32)> = vec![(root, 0)];
    while let Some((node_id, level)) = stack.pop() {
        if flat.len() > tpk.nodes.len() || level > 255 {
            return None;
        }
        let node = tpk.nodes.get(node_id as usize)?;
        let m_type = tpk.strings.get(node.type_name as usize)?.clone();
        let m_name = tpk.strings.get(node.name as usize)?.clone();
        flat.push((
            level as u8,
            TypeTree {
                m_type,
                m_name,
                m_version: node.version as i32,
                m_type_flags: node.type_flags as i32,
                m_byte_size: node.byte_size,
                m_meta_flag: node.meta_flag as i32,
                children: Vec::new(),
            },
        ));
        for &sub_node in node.sub_nodes.iter().rev() {
            stack.push((sub_node, level + 1));
        }
    }

    build_tree_from_levels(flat).ok()
}

/// 查询 blob 字符串缓冲区里引用的 common string。
pub(crate) fn common_string(offset: u32) -> Option<String> {
    embedded_tpk()?.common_strings.get(&offset).cloned()
}

fn embedded_tpk() -> Option<&'static Tpk> {
    static CACHE: OnceLock<Option<Tpk>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let payload = decompress_container(EMBEDDED_TPK).ok()?;
            Tpk::parse(&payload).ok()
        })
        .as_ref()
}

struct TpkContainer<'a> {
    compression: u8,
    payload: &'a [u8],
    uncompressed_size: usize,
}

impl<'a> TpkContainer<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, UnityError> {
        let mut reader = ByteReader::new(bytes);
        let magic = reader.read_bytes(4)?;
        if magic != TPK_MAGIC {
            return Err(UnityError::Parse("invalid TPK magic".into()));
        }
        let version = reader.read_u8()?;
        if version != TPK_VERSION {
            return Err(UnityError::Unsupported(format!("unsupported TPK version {version}")));
        }
        let compression = reader.read_u8()?;
        let data_type = reader.read_u8()?;
        if data_type != DATA_TYPE_TYPETREE {
            return Err(UnityError::Unsupported(format!("unsupported TPK data type {data_type}")));
        }
        reader.read_u8()?;
        reader.read_u32()?;

        let compressed_size = reader.read_i32()?;
        let uncompressed_size = reader.read_i32()?;
        if compressed_size < 0 || uncompressed_size < 0 {
            return Err(UnityError::Parse("negative TPK payload size".into()));
        }
        let compressed_size = compressed_size as usize;
        let uncompressed_size = uncompressed_size as usize;
        let payload = reader.read_bytes(compressed_size)?;
        Ok(Self { compression, payload, uncompressed_size })
    }
}

struct TpkUnityClass {
    release_root_node: Option<u16>,
}

struct TpkUnityNode {
    type_name: u16,
    name: u16,
    byte_size: i32,
    version: i16,
    type_flags: u8,
    meta_flag: u32,
    sub_nodes: Vec<u16>,
}

struct Tpk {
    classes: HashMap<i32, Vec<(UnityVersion, Option<TpkUnityClass>)>>,
    common_strings: HashMap<u32, String>,
    nodes: Vec<TpkUnityNode>,
    strings: Vec<String>,
}

impl Tpk {
    fn parse(payload: &[u8]) -> Result<Self, UnityError> {
        let mut reader = ByteReader::new(payload);
        let _creation_time = reader.read_i64()?;

        let version_count = read_count(&mut reader, 8, "TPK version count")?;
        for _ in 0..version_count {
            read_tpk_version(&mut reader)?;
        }

        let class_count = read_count(&mut reader, 8, "TPK class count")?;
        let mut classes = HashMap::with_capacity(class_count.min(4096));
        for _ in 0..class_count {
            let class_id = reader.read_i32()?;
            let entries = read_count(&mut reader, 8, "TPK class entries")?;
            let mut class_entries = Vec::with_capacity(entries.min(1024));
            for _ in 0..entries {
                let version = read_tpk_version(&mut reader)?;
                let has_data = reader.read_u8()? != 0;
                let class = if has_data { Some(read_tpk_class(&mut reader)?) } else { None };
                class_entries.push((version, class));
            }
            classes.insert(class_id, class_entries);
        }

        let common_indices = read_tpk_common_string(&mut reader)?;

        let node_count = read_count(&mut reader, 17, "TPK node count")?;
        let mut nodes = Vec::with_capacity(node_count.min(65536));
        for _ in 0..node_count {
            nodes.push(read_tpk_node(&mut reader)?);
        }

        let string_count = read_count(&mut reader, 1, "TPK string count")?;
        let mut strings = Vec::with_capacity(string_count.min(65536));
        for _ in 0..string_count {
            strings.push(read_7bit_string(&mut reader)?);
        }

        let common_strings = build_common_string_offsets(&common_indices, &strings);

        Ok(Self { classes, common_strings, nodes, strings })
    }
}

fn build_common_string_offsets(indices: &[u16], strings: &[String]) -> HashMap<u32, String> {
    let mut map = HashMap::with_capacity(indices.len());
    let mut offset = 0u32;
    for &index in indices {
        let Some(value) = strings.get(index as usize)
        else {
            break;
        };
        map.insert(offset, value.clone());
        offset = offset.saturating_add(value.len() as u32 + 1);
    }
    map
}

fn read_tpk_version(reader: &mut ByteReader<'_>) -> Result<UnityVersion, UnityError> {
    let packed = reader.read_u64()?;
    let major = ((packed >> 48) & 0xFFFF) as u32;
    let minor = ((packed >> 32) & 0xFFFF) as u32;
    let build = ((packed >> 16) & 0xFFFF) as u32;
    let kind_code = ((packed >> 8) & 0xFF) as u8;
    let revision = (packed & 0xFF) as u32;
    Ok(unity_version_from_parts(major, minor, build, kind_code, revision))
}

fn read_tpk_class(reader: &mut ByteReader<'_>) -> Result<TpkUnityClass, UnityError> {
    let _name = reader.read_u16()?;
    let _base = reader.read_u16()?;
    let flags = reader.read_u8()?;
    if flags & 0x40 != 0 {
        reader.read_u16()?;
    }
    let release_root_node = if flags & 0x80 != 0 { Some(reader.read_u16()?) } else { None };
    Ok(TpkUnityClass { release_root_node })
}

fn read_tpk_node(reader: &mut ByteReader<'_>) -> Result<TpkUnityNode, UnityError> {
    let type_name = reader.read_u16()?;
    let name = reader.read_u16()?;
    let byte_size = reader.read_i32()?;
    let version = reader.read_i16()?;
    let type_flags = reader.read_u8()?;
    let meta_flag = reader.read_u32()?;
    let sub_node_count = reader.read_u16()? as usize;
    if sub_node_count * 2 > reader.remaining() {
        return Err(UnityError::Parse("TPK node child count exceeds remaining bytes".into()));
    }
    let mut sub_nodes = Vec::with_capacity(sub_node_count);
    for _ in 0..sub_node_count {
        sub_nodes.push(reader.read_u16()?);
    }
    Ok(TpkUnityNode { type_name, name, byte_size, version, type_flags, meta_flag, sub_nodes })
}

fn read_tpk_common_string(reader: &mut ByteReader<'_>) -> Result<Vec<u16>, UnityError> {
    let version_count = read_count(reader, 9, "TPK common string version count")?;
    for _ in 0..version_count {
        read_tpk_version(reader)?;
        reader.read_u8()?;
    }

    let index_count = read_count(reader, 2, "TPK common string index count")?;
    let mut indices = Vec::with_capacity(index_count.min(65536));
    for _ in 0..index_count {
        indices.push(reader.read_u16()?);
    }
    Ok(indices)
}

fn read_7bit_string(reader: &mut ByteReader<'_>) -> Result<String, UnityError> {
    let mut result = 0usize;
    let mut shift = 0u32;
    loop {
        let byte = reader.read_u8()?;
        result |= ((byte & 0x7F) as usize) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 35 {
            return Err(UnityError::Parse("TPK 7-bit length is too long".into()));
        }
    }
    let bytes = reader.read_bytes(result)?;
    String::from_utf8(bytes.to_vec()).map_err(|error| UnityError::Parse(format!("invalid TPK string: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::parse_unity_version;

    #[test]
    fn embedded_tpk_decompresses() {
        let container = TpkContainer::parse(EMBEDDED_TPK).unwrap();
        assert_eq!(container.compression, COMPRESSION_LZ4);
        let payload = decompress_container(EMBEDDED_TPK).unwrap();
        assert_eq!(payload.len(), container.uncompressed_size);
        assert!(payload.len() > 100_000);
    }

    #[test]
    fn embedded_tpk_contains_common_classes() {
        let version = parse_unity_version("2020.3.41f1").unwrap();
        let transform = lookup_typetree(4, &version).expect("Transform typetree");
        assert!(!transform.children.is_empty());
        assert_eq!(transform.m_type, "Transform");

        let game_object = lookup_typetree(1, &version).expect("GameObject typetree");
        assert!(!game_object.children.is_empty());
        assert_eq!(game_object.m_type, "GameObject");
    }

    #[test]
    fn unknown_class_returns_none() {
        let version = parse_unity_version("2020.3.41f1").unwrap();
        assert!(lookup_typetree(-4242, &version).is_none());
    }

    #[test]
    fn reconstructs_common_string_table() {
        assert_eq!(common_string(0).as_deref(), Some("AABB"));
        assert_eq!(common_string(427).as_deref(), Some("m_Name"));
        assert_eq!(common_string(840).as_deref(), Some("string"));
        assert_eq!(common_string(914).as_deref(), Some("UInt32"));
        assert!(common_string(u32::MAX).is_none());
    }
}
