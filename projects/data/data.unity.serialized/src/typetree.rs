//! TypeTree 节点、blob 解析与类型分类。
//!
//! 两种序列化形式都支持：
//! - `version >= 12` 或 `version == 10` 的 blob：节点表 + 字符串缓冲区，字符串可
//!   引用包内 common string 表；
//! - 更早版本的递归节点序列。

use crate::{UnityError, reader::ByteReader, tpk};

/// TypeTree 的安全深度上限。
const MAX_TREE_DEPTH: usize = 256;

/// 一个 TypeTree 节点。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TypeTree {
    pub(crate) m_type: String,
    pub(crate) m_name: String,
    pub(crate) m_version: i32,
    pub(crate) m_type_flags: i32,
    pub(crate) m_byte_size: i32,
    pub(crate) m_meta_flag: i32,
    pub(crate) children: Vec<TypeTree>,
}

/// 节点语义分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Bool,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    Float,
    Double,
    Char,
    String,
    Map,
    Untyped,
    Empty,
    ArrayWrapper,
    Array,
    Struct,
    TodoType,
}

impl TypeTree {
    /// `m_MetaFlag` 的 `ALIGN_BYTES_FLAG`。
    pub(crate) fn requires_align(&self) -> bool {
        (self.m_meta_flag & 0x4000) != 0
    }

    /// 按字段类型归类。
    pub(crate) fn classify(&self) -> Kind {
        use Kind::*;
        match self.m_type.as_str() {
            "bool" => Bool,
            "UInt8" => U8,
            "UInt16" | "unsigned short" => U16,
            "UInt32" | "unsigned int" | "Type*" => U32,
            "UInt64" | "unsigned long long" | "FileSize" => U64,
            "SInt8" => I8,
            "SInt16" | "short" => I16,
            "SInt32" | "int" => I32,
            "SInt64" | "long long" => I64,
            "float" => Float,
            "double" => Double,
            "char" => Char,
            "string" => String,
            "map" => Map,
            "Type" => TodoType,
            "Array" => Array,
            "TypelessData" => Untyped,
            _ => match self.children.len() {
                0 => Empty,
                1 if self.children[0].m_type == "Array" => ArrayWrapper,
                _ => Struct,
            },
        }
    }
}

/// 解析内嵌的 blob 形式 TypeTree。
pub(crate) fn parse_blob(reader: &mut ByteReader<'_>, version: u32) -> Result<TypeTree, UnityError> {
    let node_size = if version >= 19 { 32usize } else { 24usize };
    let node_count = read_non_negative(reader, "typetree node count")?;
    let string_size = read_non_negative(reader, "typetree string buffer size")?;

    let node_bytes_len = node_count.checked_mul(node_size).ok_or_else(|| UnityError::Parse("typetree node table overflow".into()))?;
    let nodes_bytes = reader.read_bytes(node_bytes_len)?;
    let strings = reader.read_bytes(string_size)?;

    let mut nodes = Vec::with_capacity(node_count.min(4096));
    let mut node_reader = ByteReader::new(nodes_bytes);
    node_reader.set_little(reader.little());
    for _ in 0..node_count {
        let m_version = node_reader.read_u16()? as i32;
        let level = node_reader.read_u8()?;
        let type_flags = node_reader.read_u8()? as i32;
        let type_offset = node_reader.read_u32()?;
        let name_offset = node_reader.read_u32()?;
        let byte_size = node_reader.read_i32()?;
        let _index = node_reader.read_i32()?;
        let meta_flag = node_reader.read_i32()?;
        if version >= 19 {
            let _ref_type_hash = node_reader.read_u64()?;
        }

        nodes.push((
            level,
            TypeTree {
                m_type: resolve_string(type_offset, strings)?,
                m_name: resolve_string(name_offset, strings)?,
                m_version,
                m_type_flags: type_flags,
                m_byte_size: byte_size,
                m_meta_flag: meta_flag,
                children: Vec::new(),
            },
        ));
    }

    build_tree_from_levels(nodes)
}

/// 解析旧版本的递归节点序列。
pub(crate) fn parse_node_sequence(reader: &mut ByteReader<'_>, version: u32) -> Result<TypeTree, UnityError> {
    parse_node(reader, version, 0)
}

fn parse_node(reader: &mut ByteReader<'_>, version: u32, depth: usize) -> Result<TypeTree, UnityError> {
    if depth > MAX_TREE_DEPTH {
        return Err(UnityError::Parse("typetree nesting is too deep".into()));
    }

    let m_type = reader.read_cstr()?;
    let m_name = reader.read_cstr()?;
    let m_byte_size = reader.read_i32()?;
    if version == 2 {
        let _variable_count = reader.read_i32()?;
    }
    if version != 3 {
        let _index = reader.read_i32()?;
    }
    let m_type_flags = reader.read_i32()?;
    let m_version = reader.read_i32()?;
    let m_meta_flag = if version != 3 { reader.read_i32()? } else { 0 };

    let child_count = read_non_negative(reader, "typetree child count")?;
    if child_count > reader.remaining() {
        return Err(UnityError::Parse(format!("typetree child count {child_count} exceeds remaining bytes")));
    }
    let mut children = Vec::with_capacity(child_count.min(1024));
    for _ in 0..child_count {
        children.push(parse_node(reader, version, depth + 1)?);
    }

    Ok(TypeTree { m_type, m_name, m_version, m_type_flags, m_byte_size, m_meta_flag, children })
}

/// 用 `(level, node)` 的前序扁平序列重建树。
pub(crate) fn build_tree_from_levels(nodes: Vec<(u8, TypeTree)>) -> Result<TypeTree, UnityError> {
    let Some((root_level, _)) = nodes.first()
    else {
        return Err(UnityError::Parse("typetree contains no nodes".into()));
    };
    if *root_level != 0 {
        return Err(UnityError::Parse("typetree root node is not at level 0".into()));
    }

    let mut index = 0usize;
    let root = build_subtree(&nodes, &mut index, 0)?;
    if index != nodes.len() {
        return Err(UnityError::Parse("typetree contains unreachable nodes".into()));
    }
    Ok(root)
}

fn build_subtree(nodes: &[(u8, TypeTree)], index: &mut usize, level: u8) -> Result<TypeTree, UnityError> {
    let (node_level, node) = nodes.get(*index).ok_or_else(|| UnityError::Parse("typetree nesting is truncated".into()))?;
    if *node_level != level {
        return Err(UnityError::Parse(format!("invalid typetree level {} at node {index}", node_level)));
    }
    let mut node = node.clone();
    *index += 1;

    while let Some((child_level, _)) = nodes.get(*index) {
        if *child_level <= level {
            break;
        }
        if *child_level != level.saturating_add(1) {
            return Err(UnityError::Parse(format!("invalid typetree level {} at node {index}", child_level)));
        }
        node.children.push(build_subtree(nodes, index, level + 1)?);
    }

    Ok(node)
}

fn read_non_negative(reader: &mut ByteReader<'_>, what: &str) -> Result<usize, UnityError> {
    let raw = reader.read_i32()?;
    if raw < 0 {
        return Err(UnityError::Parse(format!("negative {what}: {raw}")));
    }
    Ok(raw as usize)
}

fn resolve_string(value: u32, strings: &[u8]) -> Result<String, UnityError> {
    if value & 0x8000_0000 == 0 {
        let offset = value as usize;
        let tail = strings.get(offset..).ok_or_else(|| UnityError::Parse("typetree string offset out of range".into()))?;
        let len = tail.iter().position(|&byte| byte == 0).ok_or_else(|| UnityError::Parse("unterminated typetree string".into()))?;
        String::from_utf8(tail[..len].to_vec()).map_err(|error| UnityError::Parse(format!("invalid typetree string: {error}")))
    }
    else {
        let offset = value & 0x7FFF_FFFF;
        tpk::common_string(offset).ok_or_else(|| UnityError::Parse(format!("unknown common string offset {offset}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(ty: &str, name: &str) -> TypeTree {
        TypeTree { m_type: ty.to_string(), m_name: name.to_string(), ..Default::default() }
    }

    #[test]
    fn classifies_known_types() {
        assert_eq!(node("bool", "x").classify(), Kind::Bool);
        assert_eq!(node("UInt32", "x").classify(), Kind::U32);
        assert_eq!(node("string", "x").classify(), Kind::String);
        assert_eq!(node("map", "x").classify(), Kind::Map);
        assert_eq!(node("TypelessData", "x").classify(), Kind::Untyped);
        assert_eq!(node("Type", "x").classify(), Kind::TodoType);
        assert_eq!(node("Vector3f", "x").classify(), Kind::Empty);

        let wrapper = TypeTree { m_type: "vector".into(), children: vec![node("Array", "Array")], ..Default::default() };
        assert_eq!(wrapper.classify(), Kind::ArrayWrapper);

        let structure = TypeTree { m_type: "GameObject".into(), children: vec![node("UInt32", "m_Layer")], ..Default::default() };
        assert_eq!(structure.classify(), Kind::Struct);
    }

    #[test]
    fn rebuilds_tree_from_levels() {
        let nodes = vec![(0, node("Root", "Base")), (1, node("UInt32", "a")), (1, node("Nested", "b")), (2, node("UInt32", "c"))];
        let tree = build_tree_from_levels(nodes).unwrap();
        assert_eq!(tree.children.len(), 2);
        assert_eq!(tree.children[1].children[0].m_name, "c");
    }

    #[test]
    fn rejects_bad_levels() {
        let nodes = vec![(0, node("Root", "Base")), (2, node("UInt32", "a"))];
        assert!(build_tree_from_levels(nodes).is_err());
    }

    #[test]
    fn resolves_common_strings_in_blob() {
        let mut node_bytes = Vec::new();
        node_bytes.extend_from_slice(&1u16.to_le_bytes());
        node_bytes.push(0); // level
        node_bytes.push(0); // flags
        node_bytes.extend_from_slice(&(0x8000_0000u32 | 914).to_le_bytes()); // UInt32
        node_bytes.extend_from_slice(&(0x8000_0000u32 | 427).to_le_bytes()); // m_Name
        node_bytes.extend_from_slice(&(-1i32).to_le_bytes());
        node_bytes.extend_from_slice(&0i32.to_le_bytes());
        node_bytes.extend_from_slice(&0i32.to_le_bytes());
        node_bytes.extend_from_slice(&0u64.to_le_bytes());

        let mut blob = Vec::new();
        blob.extend_from_slice(&1i32.to_le_bytes());
        blob.extend_from_slice(&0i32.to_le_bytes());
        blob.extend_from_slice(&node_bytes);

        let mut reader = ByteReader::new(&blob);
        let tree = parse_blob(&mut reader, 22).unwrap();
        assert_eq!(tree.m_type, "UInt32");
        assert_eq!(tree.m_name, "m_Name");
    }
}
