//! `PE` 资源目录枚举。
//!
//! 走三级 `IMAGE_RESOURCE_DIRECTORY` 树（类型、名称、语言），报告叶子数据项。
//! 资源目录的内容不在 [`PeImage`](super::read::PeImage) 里，因此在这里解码。

use serde::{Deserialize, Serialize};

use super::{
    analysis::PeAnalysisError,
    read::{PeImage, rva_to_offset},
};

/// 资源表在数据目录中的索引。
const DIRECTORY_RESOURCE: usize = 2;
/// 最多保留的叶子项。
const MAX_RESOURCES: usize = 256;
/// 最多访问的目录节点。
const MAX_NODES: usize = 8192;
/// 目录树最多下钻的层数，避免环状树耗尽栈。
const MAX_DEPTH: usize = 8;

/// 资源树的一个叶子数据项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeResource {
    /// 资源类型：标准名（`VERSION`、`MANIFEST` 等）、自定义名或 `#<id>`。
    pub resource_type: String,
    /// 资源名：自定义名或 `#<id>`。
    pub name: String,
    /// 语言 id；语言层使用字符串名时为 `0`。
    pub language: u32,
    /// 载荷 `RVA`。
    pub data_rva: u32,
    /// 载荷字节数。
    pub size: u32,
    /// 数据项记录的代码页。
    pub code_page: u32,
}

/// 有界的资源目录列表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeResourcesReport {
    /// 镜像是否有资源目录。
    pub present: bool,
    /// 通过上限的叶子项，按树序排列。
    pub resources: Vec<PeResource>,
    /// 树声明的叶子项总数。
    pub total: usize,
    /// 列表是否被上限或节点预算截断。
    pub truncated: bool,
}

/// 解码一个 PE 镜像的资源目录。
pub fn read_resources(image: &PeImage, bytes: &[u8]) -> Result<PeResourcesReport, PeAnalysisError> {
    let directory = image.optional.data_directories[DIRECTORY_RESOURCE];
    if directory.rva == 0 || directory.size == 0 {
        return Ok(PeResourcesReport { present: false, resources: Vec::new(), total: 0, truncated: false });
    }
    let base =
        usize::try_from(rva_to_offset(&image.sections, directory.rva).map_err(|_| PeAnalysisError::Rva(directory.rva))?).unwrap_or(usize::MAX);

    let mut walk = Walk { bytes, base, visited: 0, total: 0, resources: Vec::new(), truncated: false };
    let mut path = Vec::new();
    walk.directory(0, 0, &mut path)?;

    Ok(PeResourcesReport { present: true, resources: walk.resources, total: walk.total, truncated: walk.truncated })
}

enum Node {
    Id(u32),
    Name(String),
}

struct Walk<'a> {
    bytes: &'a [u8],
    base: usize,
    visited: usize,
    total: usize,
    resources: Vec<PeResource>,
    truncated: bool,
}

impl Walk<'_> {
    fn directory(&mut self, offset: u32, depth: usize, path: &mut Vec<Node>) -> Result<(), PeAnalysisError> {
        self.visited += 1;
        if self.visited > MAX_NODES || depth > MAX_DEPTH {
            self.truncated = true;
            return Ok(());
        }
        let at = self.base.saturating_add(offset as usize);
        let named = read_u16(self.bytes, at + 12)?;
        let ids = read_u16(self.bytes, at + 14)?;
        for index in 0..usize::from(named).saturating_add(usize::from(ids)) {
            let entry = at.saturating_add(16).saturating_add(index.saturating_mul(8));
            let name_field = read_u32(self.bytes, entry)?;
            let target = read_u32(self.bytes, entry + 4)?;
            let node = if name_field & 0x8000_0000 != 0 { Node::Name(self.string(name_field & 0x7fff_ffff)?) } else { Node::Id(name_field) };
            if target & 0x8000_0000 != 0 {
                path.push(node);
                self.directory(target & 0x7fff_ffff, depth + 1, path)?;
                path.pop();
            }
            else if depth >= 2 {
                self.leaf(target, path, node)?;
            }
        }
        Ok(())
    }

    fn leaf(&mut self, offset: u32, path: &[Node], language: Node) -> Result<(), PeAnalysisError> {
        self.total += 1;
        if self.resources.len() >= MAX_RESOURCES {
            self.truncated = true;
            return Ok(());
        }
        let Some(resource_type) = path.first()
        else {
            return Ok(());
        };
        let Some(name) = path.get(1)
        else {
            return Ok(());
        };
        let at = self.base.saturating_add(offset as usize);
        let data_rva = read_u32(self.bytes, at)?;
        let size = read_u32(self.bytes, at + 4)?;
        let code_page = read_u32(self.bytes, at + 8)?;
        self.resources.push(PeResource {
            resource_type: render_type(resource_type),
            name: render_name(name),
            language: match language {
                Node::Id(id) => id,
                Node::Name(_) => 0,
            },
            data_rva,
            size,
            code_page,
        });
        Ok(())
    }

    fn string(&self, offset: u32) -> Result<String, PeAnalysisError> {
        let at = self.base.saturating_add(offset as usize);
        let length = usize::from(read_u16(self.bytes, at)?);
        let start = at.saturating_add(2);
        let end = start.saturating_add(length.saturating_mul(2));
        let raw = self.bytes.get(start..end).ok_or(PeAnalysisError::Truncated { table: "资源目录字符串" })?;
        let mut units = Vec::with_capacity(length);
        for index in 0..length {
            units.push(u16::from_le_bytes([raw[index * 2], raw[index * 2 + 1]]));
        }
        Ok(String::from_utf16_lossy(&units))
    }
}

fn render_type(node: &Node) -> String {
    match node {
        Node::Name(name) => name.clone(),
        Node::Id(id) => type_name(*id).map_or_else(|| format!("#{id}"), str::to_string),
    }
}

fn render_name(node: &Node) -> String {
    match node {
        Node::Name(name) => name.clone(),
        Node::Id(id) => format!("#{id}"),
    }
}

fn type_name(id: u32) -> Option<&'static str> {
    Some(match id {
        1 => "CURSOR",
        2 => "BITMAP",
        3 => "ICON",
        4 => "MENU",
        5 => "DIALOG",
        6 => "STRING",
        7 => "FONTDIR",
        8 => "FONT",
        9 => "ACCELERATOR",
        10 => "RCDATA",
        11 => "MESSAGETABLE",
        12 => "GROUP_CURSOR",
        14 => "GROUP_ICON",
        16 => "VERSION",
        17 => "DLGINCLUDE",
        19 => "PLUGPLAY",
        20 => "VXD",
        21 => "ANICURSOR",
        22 => "ANIICON",
        23 => "HTML",
        24 => "MANIFEST",
        _ => return None,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, PeAnalysisError> {
    let raw: [u8; 2] = bytes
        .get(offset..offset.saturating_add(2))
        .and_then(|slice| slice.try_into().ok())
        .ok_or(PeAnalysisError::Truncated { table: "资源目录" })?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, PeAnalysisError> {
    let raw: [u8; 4] = bytes
        .get(offset..offset.saturating_add(4))
        .and_then(|slice| slice.try_into().ok())
        .ok_or(PeAnalysisError::Truncated { table: "资源目录" })?;
    Ok(u32::from_le_bytes(raw))
}
