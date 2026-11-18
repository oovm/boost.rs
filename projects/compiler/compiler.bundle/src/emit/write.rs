//! 产物写出：本层不直接触碰文件系统，只消费宿主注入的 [`OutputSink`]。
//!
//! 单文件失败记录诊断并继续尝试其余文件，最终整体 `Err`——产物集必须完整，
//! 不接受半套输出被当作成功。
//!
//! 两种模式共享同一份「产物闭包」定义：
//! - [`write_emit_with`]：全量写，每个产物都落盘；
//! - [`write_emit_incremental_with`]：`reuse = true` 时逐字节比较已存在文件，
//!   内容相同即跳过写（复用判定只看内容，不看 mtime / 时间戳）。
//!
//! 两种模式都会删除输出目录内不再属于闭包的旧产物（chunk / map / css 与
//! `assets/` 下按内容指纹命名的资源），不留孤儿。删除是逐文件的非递归操作，
//! 且不触碰缓存目录与其它外来文件（如 Node 侧 `package.json`、宿主自行放进
//! `assets/` 的非指纹文件）。

use std::collections::BTreeSet;
use std::path::Path;

use crate::diag::PackDiagnostic;

use super::EmitPlan;

/// 产物写出的宿主端口：文件系统事务归调用方。
pub trait OutputSink {
    /// 递归创建目录。
    fn create_dir_all(&self, path: &Path) -> std::io::Result<()>;
    /// 读取既有文件（复用判定用）。
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>>;
    /// 写入整段字节。
    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()>;
    /// 复制文件。
    fn copy(&self, from: &Path, to: &Path) -> std::io::Result<u64>;
    /// 列出目录条目：`(file_name, is_file)`。
    fn list(&self, directory: &Path) -> std::io::Result<Vec<(String, bool)>>;
    /// 删除单个文件。
    fn remove_file(&self, path: &Path) -> std::io::Result<()>;
    /// 文件是否存在。
    fn is_file(&self, path: &Path) -> bool;
}

/// 全量写出：等价于 [`write_emit_incremental_with`] 的 `reuse = false`。
pub fn write_emit_with(sink: &dyn OutputSink, plan: &EmitPlan, out_dir: &Path) -> Result<(), Vec<PackDiagnostic>> {
    write_emit_incremental_with(sink, plan, out_dir, false)
}

/// 增量写出：`reuse = true` 时跳过内容逐字节相同的既有文件。
pub fn write_emit_incremental_with(
    sink: &dyn OutputSink,
    plan: &EmitPlan,
    out_dir: &Path,
    reuse: bool,
) -> Result<(), Vec<PackDiagnostic>> {
    let mut diagnostics: Vec<PackDiagnostic> = Vec::new();
    if let Err(err) = sink.create_dir_all(out_dir) {
        diagnostics.push(write_failed(out_dir, &err));
    }
    let mut expected: BTreeSet<String> = BTreeSet::new();
    for artifact in &plan.artifacts {
        expected.insert(artifact.file_name.clone());
        let map_name = format!("{}.map", artifact.file_name);
        expected.insert(map_name.clone());
        write_bytes(sink, out_dir, &artifact.file_name, artifact.code.as_bytes(), reuse, &mut diagnostics);
        write_bytes(sink, out_dir, &map_name, artifact.source_map.as_bytes(), reuse, &mut diagnostics);
    }
    for style in &plan.styles {
        expected.insert(style.file_name.clone());
        write_bytes(sink, out_dir, &style.file_name, style.content.as_bytes(), reuse, &mut diagnostics);
    }
    for asset in &plan.assets {
        expected.insert(asset.file_name.clone());
        let target = out_dir.join(&asset.file_name);
        if let Some(parent) = target.parent()
            && let Err(err) = sink.create_dir_all(parent)
        {
            diagnostics.push(write_failed(parent, &err));
            continue;
        }
        // 资源文件名已含源内容指纹：同名即同内容，复用模式无需重拷。
        if reuse && sink.is_file(&target) {
            continue;
        }
        if let Err(err) = sink.copy(Path::new(&asset.source_path), &target) {
            diagnostics.push(write_failed(&target, &err));
        }
    }
    expected.insert("manifest.json".to_string());
    write_bytes(sink, out_dir, "manifest.json", plan.manifest_json.as_bytes(), reuse, &mut diagnostics);
    sweep_orphans(sink, out_dir, &expected, &mut diagnostics);
    if diagnostics.is_empty() { Ok(()) } else { Err(diagnostics) }
}

/// 写单文件；`reuse` 且既有字节完全相同时跳过。
fn write_bytes(sink: &dyn OutputSink, out_dir: &Path, file_name: &str, bytes: &[u8], reuse: bool, diagnostics: &mut Vec<PackDiagnostic>) {
    let path = out_dir.join(file_name);
    if reuse
        && let Ok(existing) = sink.read(&path)
        && existing == bytes
    {
        return;
    }
    if let Some(parent) = path.parent()
        && let Err(err) = sink.create_dir_all(parent)
    {
        diagnostics.push(write_failed(parent, &err));
        return;
    }
    if let Err(err) = sink.write(&path, bytes) {
        diagnostics.push(write_failed(&path, &err));
    }
}

/// 清理不属于本次产物闭包的旧文件：
/// - 输出目录根：只删 `*.js` / `*.map` / `*.css` 形态的旧产物；
/// - `assets/`：只删 `<stem>-<hex16>.<ext>` 形态且不在闭包内的旧资源；
/// - 其它子目录（含缓存目录）与非产物形态文件一律不碰。
///
/// 全部是逐文件删除，不做递归目录删除。
fn sweep_orphans(sink: &dyn OutputSink, out_dir: &Path, expected: &BTreeSet<String>, diagnostics: &mut Vec<PackDiagnostic>) {
    if out_dir.file_name().is_none() {
        // 防御性拒绝：根路径不允许被当作产物目录清理。
        return;
    }
    let Ok(entries) = sink.list(out_dir) else {
        return;
    };
    for (name, is_file) in entries {
        let path = out_dir.join(&name);
        if !is_file {
            if name == "assets" {
                sweep_dir_files(sink, &path, "assets/", expected, diagnostics);
            }
            continue;
        }
        if expected.contains(&name) || !is_artifact_name(&name) {
            continue;
        }
        if let Err(err) = sink.remove_file(&path) {
            diagnostics.push(write_failed(&path, &err));
        }
    }
}

fn sweep_dir_files(sink: &dyn OutputSink, dir: &Path, prefix: &str, expected: &BTreeSet<String>, diagnostics: &mut Vec<PackDiagnostic>) {
    let Ok(entries) = sink.list(dir) else {
        return;
    };
    for (file_name, is_file) in entries {
        if !is_file || !is_asset_name(&file_name) {
            continue;
        }
        if expected.contains(&format!("{prefix}{file_name}")) {
            continue;
        }
        if let Err(err) = sink.remove_file(&dir.join(&file_name)) {
            diagnostics.push(write_failed(&dir.join(&file_name), &err));
        }
    }
}

/// 产物形态判定：只为避免误删外来文件，不承担闭包定义。
fn is_artifact_name(name: &str) -> bool {
    name.ends_with(".js") || name.ends_with(".map") || name.ends_with(".css")
}

/// `assets/` 产物形态：`<stem>-<hex16>.<ext>`（源内容指纹定名）。
/// 不带 16 位十六进制后缀的文件不是 pack 产物，清扫不碰。
fn is_asset_name(name: &str) -> bool {
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    stem.rsplit_once('-').is_some_and(|(_, hash)| hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit()))
}

fn write_failed(path: &Path, err: &std::io::Error) -> PackDiagnostic {
    PackDiagnostic::new("vmz::pack::emit::write_failed", format!("cannot write artifact: {err}")).with_path(path.display().to_string())
}
