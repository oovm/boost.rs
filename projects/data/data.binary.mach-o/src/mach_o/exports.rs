//! `dyld` 导出 trie：来自 `LC_DYLD_EXPORTS_TRIE` 或 `LC_DYLD_INFO` 的
//! `export_off` / `export_size`。

use super::leb::read_uleb;
use super::read::MachOImage;

const MAX_EXPORTS: usize = 512;
const MAX_EXPORT_NODES: usize = 8192;
const MAX_EXPORT_DEPTH: usize = 64;

/// 从导出 trie 解码出的一条符号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachExport {
    /// 累积后的符号名。
    pub name: String,
    /// 原始导出标志。
    pub flags: u64,
    /// 标志名：`WEAK`、`REEXPORT`、`STUB_AND_RESOLVER`。
    pub flag_names: Vec<&'static str>,
    /// 常规导出的符号地址。
    pub address: Option<u64>,
    /// reexport 的库序号或 stub-and-resolver 的 stub 地址。
    pub other: Option<u64>,
}

/// 导出 trie 的有界列表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachExports {
    /// 镜像是否带有导出 trie。
    pub present: bool,
    /// 按 trie 顺序解码的导出。
    pub exports: Vec<MachExport>,
    /// 存储上限之前发现的条目数量。
    pub total: usize,
    /// 是否因上限或畸形节点截断。
    pub truncated: bool,
}

/// 解码 `Mach-O` 的导出 trie。
pub fn read_exports(bytes: &[u8], image: &MachOImage) -> MachExports {
    let Some((data_offset, data_size)) = export_range(image)
    else {
        return MachExports { present: false, exports: Vec::new(), total: 0, truncated: false };
    };
    let start = data_offset as usize;
    let data = bytes.get(start..start.saturating_add(data_size as usize)).unwrap_or(&[]);

    let mut report = MachExports { present: true, exports: Vec::new(), total: 0, truncated: start.saturating_add(data_size as usize) > bytes.len() };
    let mut nodes = 0usize;
    let mut stack = vec![(0usize, String::new(), 0usize)];
    while let Some((node, prefix, depth)) = stack.pop() {
        nodes += 1;
        if nodes > MAX_EXPORT_NODES || depth > MAX_EXPORT_DEPTH || report.exports.len() >= MAX_EXPORTS {
            report.truncated = true;
            continue;
        }
        let Some((terminal_size, terminal_start)) = read_uleb(data, node)
        else {
            report.truncated = true;
            continue;
        };
        let child_start = terminal_start.saturating_add(terminal_size as usize);
        if terminal_size > 0 {
            let Some((flags, next)) = read_uleb(data, terminal_start)
            else {
                report.truncated = true;
                continue;
            };
            let cursor = next;
            let (address, other) = if flags & 0x02 != 0 || flags & 0x04 != 0 {
                let Some((other, _)) = read_uleb(data, cursor)
                else {
                    report.truncated = true;
                    continue;
                };
                (None, Some(other))
            }
            else {
                let Some((address, _)) = read_uleb(data, cursor)
                else {
                    report.truncated = true;
                    continue;
                };
                (Some(address), None)
            };
            report.total += 1;
            if report.exports.len() < MAX_EXPORTS {
                report.exports.push(MachExport {
                    name: prefix.clone(),
                    flags,
                    flag_names: export_flag_names(flags),
                    address,
                    other,
                });
            }
            else {
                report.truncated = true;
            }
        }
        let Some(children) = data.get(child_start)
        else {
            report.truncated = true;
            continue;
        };
        let mut position = child_start + 1;
        let mut pending = Vec::new();
        for _ in 0..*children {
            let Some(end) = data.get(position..).and_then(|rest| rest.iter().position(|byte| *byte == 0))
            else {
                report.truncated = true;
                break;
            };
            let edge = String::from_utf8_lossy(&data[position..position + end]).into_owned();
            position += end + 1;
            let Some((child, next)) = read_uleb(data, position)
            else {
                report.truncated = true;
                break;
            };
            position = next;
            pending.push((child as usize, format!("{prefix}{edge}"), depth + 1));
        }
        pending.reverse();
        stack.extend(pending);
    }
    report.truncated |= report.total > report.exports.len();
    report
}

fn export_range(image: &MachOImage) -> Option<(u32, u32)> {
    if let Some(trie) = image.exports_trie.filter(|trie| !trie.is_empty()) {
        return Some((trie.data_offset, trie.data_size));
    }
    image.dyld_info.filter(|info| info.export_size > 0).map(|info| (info.export_offset, info.export_size))
}

fn export_flag_names(flags: u64) -> Vec<&'static str> {
    const NAMES: [(u64, &str); 3] = [(0x01, "WEAK"), (0x02, "REEXPORT"), (0x04, "STUB_AND_RESOLVER")];
    NAMES.iter().filter(|(bit, _)| flags & bit != 0).map(|(_, name)| *name).collect()
}
