use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use acorn_tar::{TarEntryDraft, TarGz, write_tar};
use package_registry::sha256_hex;
use package_resolve::{PackageError, ProjectLayout};
use serde::Deserialize;

use crate::{PublishError, Result};

/// 打包后的发布产物。
#[derive(Debug, Clone)]
pub struct PackResult {
    /// gzip 压缩后的 tarball 字节。
    pub tarball_data: Vec<u8>,
    /// tarball 的 SHA-256 摘要（`sha256-<hex>`）。
    pub sha256: String,
    /// 压缩后字节长度。
    pub size: usize,
    /// tarball 内包含的文件数。
    pub file_count: usize,
}

/// 写入注册表侧车 manifest（`package.json` / `jsr.json`）的元数据。
#[derive(Debug, Clone)]
pub struct PackMeta {
    /// 写入注册表 manifest 的包名。
    pub name: String,
    /// 发布版本。
    pub version: String,
    /// 包描述。
    pub description: String,
    /// 可选许可证字段。
    pub license: Option<String>,
}

/// 打包已构建注册表产物的选项。
#[derive(Debug, Clone)]
pub struct RegistryPackOptions {
    /// `dist/{target}` 构建产物目录。
    pub artifact_dir: PathBuf,
    /// 包项目根目录（含 manifest 与额外 `files`）。
    pub package_root: PathBuf,
    /// 目标注册表 id（决定 tarball 布局规则）。
    pub registry: String,
    /// 写入侧车 manifest 的元数据。
    pub meta: PackMeta,
    /// 来自包清单 `files` 的 glob（与运行契约产物合并）。
    pub include_files: Vec<String>,
    /// 为真时 tarball 条目省略 `package/` 前缀（扁平布局）。
    pub flat_layout: bool,
    /// 调用方给出的工程布局（忽略文件名与入口别名）。
    pub layout: ProjectLayout,
}

/// 由运行契约导出的一个可发布 CLI 入口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryBinEntry {
    /// CLI 入口名（注册表 `bin` 字段键）。
    pub bin_name: String,
    /// 相对 tarball 根的入口文件路径。
    pub relative_path: String,
}

/// 简单的打包忽略匹配（精确名 / 后缀 / 前缀 `*`）。
#[derive(Debug, Clone, Default)]
pub struct PackageIgnore {
    patterns: Vec<String>,
}

impl PackageIgnore {
    /// 读取忽略文件并与内置忽略规则合并。
    pub fn load(package_path: &Path, layout: ProjectLayout) -> Self {
        let path = layout.ignore_file_path(package_path);
        let mut patterns = default_ignore_patterns(layout.lockfile);
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                patterns.push(line.to_string());
            }
        }
        Self { patterns }
    }

    /// 相对路径是否应排除在 tarball 之外。
    pub fn is_ignored(&self, relative: &str) -> bool {
        let relative = relative.replace('\\', "/");
        self.patterns.iter().any(|pattern| match_pattern(pattern, &relative))
    }
}

fn default_ignore_patterns(lockfile: &str) -> Vec<String> {
    vec![
        "vendors/".to_string(),
        ".cache/".to_string(),
        "node_modules/".to_string(),
        lockfile.to_string(),
        ".git/".to_string(),
        "target/".to_string(),
    ]
}

fn match_pattern(pattern: &str, relative: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('/') {
        return relative == prefix || relative.starts_with(&format!("{prefix}/"));
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return relative.ends_with(suffix);
    }
    relative == pattern || relative.ends_with(&format!("/{pattern}"))
}

fn targz(entries: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    let drafts: Vec<TarEntryDraft> = entries.iter().map(|(path, data)| TarEntryDraft::file(path, data.clone())).collect();
    let tar = write_tar(drafts).map_err(|error| PublishError::message(format!("failed to write tarball: {error}")))?;
    Ok(TarGz::encode(&tar))
}

/// 打包已构建的 `dist/{canonical}` 目录，供 npm / jsr 发布。
pub fn pack_registry_artifact(options: &RegistryPackOptions) -> Result<PackResult> {
    let contracts = load_run_contracts(&options.artifact_dir)?;
    let bin_entries = registry_bin_entries(&contracts, &options.artifact_dir, options.layout)?;

    let mut entries: BTreeMap<String, PathBuf> = BTreeMap::new();
    for entry in &bin_entries {
        entries.insert(entry.relative_path.clone(), options.artifact_dir.join(&entry.relative_path));
        if let Some(wasm_path) = paired_wasm_path(&entry.relative_path) {
            let wasm_file = options.artifact_dir.join(&wasm_path);
            if wasm_file.is_file() {
                entries.insert(wasm_path, wasm_file);
            }
        }
    }

    for optional in ["readme.md", "README.md", "LICENSE", "license"] {
        let path = options.package_root.join(optional);
        if path.is_file() {
            entries.insert(optional.to_string(), path);
        }
    }

    for (relative, source) in
        collect_manifest_include_paths(&options.include_files, &options.package_root, &options.artifact_dir, options.layout)?
    {
        entries.entry(relative).or_insert(source);
    }

    let prefix = |relative: &str| if options.flat_layout { relative.to_string() } else { format!("package/{relative}") };
    let mut packed: Vec<(String, Vec<u8>)> = Vec::new();
    let mut file_count = 0usize;
    for (relative, source) in &entries {
        if !source.is_file() {
            continue;
        }
        packed.push((prefix(relative), std::fs::read(source)?));
        file_count += 1;
    }

    let package_json = build_package_json(&options.meta, &bin_entries, &options.registry)?;
    packed.push((prefix("package.json"), package_json.into_bytes()));
    file_count += 1;

    if options.registry.eq_ignore_ascii_case("jsr") {
        let jsr_json = build_jsr_json(&options.meta, &bin_entries);
        packed.push((prefix("jsr.json"), jsr_json.into_bytes()));
        file_count += 1;
    }

    let buffer = targz(&packed)?;
    let sha256 = sha256_hex(&buffer);
    Ok(PackResult { size: buffer.len(), file_count, tarball_data: buffer, sha256 })
}

/// 把包目录打成 gzip tar（带 `package/` 前缀）。
pub fn pack(package_directory: &Path, meta: &PackMeta, layout: ProjectLayout) -> Result<PackResult> {
    let ignore = PackageIgnore::load(package_directory, layout);
    let files = collect_files(package_directory, &ignore)?;
    let mut packed: Vec<(String, Vec<u8>)> = Vec::new();
    for file in &files {
        let relative = file.strip_prefix(package_directory).unwrap_or(file.as_path());
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative == "package.json" {
            continue;
        }
        packed.push((format!("package/{relative}"), std::fs::read(file)?));
    }

    let package_json = serde_json::json!({
        "name": meta.name,
        "version": meta.version,
        "description": meta.description,
        "license": meta.license.clone().unwrap_or_else(|| "UNLICENSED".to_string()),
        "main": "package.nyar",
        "files": ["**/*"],
        // `package.nyar` 是程序集（Assembly）运输身份，不是 Node 执行模块。
        "nyar": {
            "schema": 1,
            "artifacts": [
                { "path": "package.nyar", "kind": "assembly", "format": "nyar-assembly" }
            ],
        },
    })
    .to_string();
    packed.push(("package/package.json".to_string(), package_json.into_bytes()));

    let buffer = targz(&packed)?;
    let sha256 = sha256_hex(&buffer);
    Ok(PackResult { size: buffer.len(), file_count: files.len().saturating_add(1), tarball_data: buffer, sha256 })
}

fn collect_manifest_include_paths(
    patterns: &[String],
    package_root: &Path,
    artifact_dir: &Path,
    layout: ProjectLayout,
) -> Result<BTreeMap<String, PathBuf>> {
    if patterns.is_empty() {
        return Ok(BTreeMap::new());
    }

    let ignore = PackageIgnore::load(package_root, layout);
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    for file in collect_files(package_root, &ignore)? {
        let relative = file.strip_prefix(package_root).unwrap_or(&file).to_string_lossy().replace('\\', "/");
        candidates.push((relative, file));
    }
    for file in list_artifact_files(artifact_dir)? {
        let artifact_relative = file.strip_prefix(artifact_dir).unwrap_or(&file).to_string_lossy().replace('\\', "/");
        if !candidates.iter().any(|(_, path)| path == &file) {
            candidates.push((artifact_relative.clone(), file.clone()));
        }
        if let Ok(root_relative) = file.strip_prefix(package_root) {
            let root_relative = root_relative.to_string_lossy().replace('\\', "/");
            if !candidates.iter().any(|(relative, path)| path == &file && relative == &root_relative) {
                candidates.push((root_relative, file));
            }
        }
    }

    let mut packed = BTreeMap::new();
    for pattern in patterns {
        let pattern = pattern.trim().replace('\\', "/");
        if pattern.is_empty() {
            continue;
        }
        for (relative, source) in &candidates {
            if !glob_match(&pattern, relative) {
                continue;
            }
            let archive_relative = if source.starts_with(artifact_dir) {
                source.strip_prefix(artifact_dir).unwrap_or(source).to_string_lossy().replace('\\', "/")
            }
            else {
                relative.clone()
            };
            packed.entry(archive_relative).or_insert_with(|| source.clone());
        }
    }
    Ok(packed)
}

/// 判断 `path` 是否匹配 `pattern`（支持 `*` 单段与 `**` 跨段通配）。
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();
    glob_match_parts(&pattern_parts, &path_parts)
}

fn glob_match_parts(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.first().copied(), path.first().copied()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some("**"), _) => {
            if pattern.len() == 1 {
                return true;
            }
            let rest = &pattern[1..];
            for index in 0..=path.len() {
                if glob_match_parts(rest, &path[index..]) {
                    return true;
                }
            }
            false
        }
        (Some(segment), Some(value)) => {
            if glob_segment_match(segment, value) {
                glob_match_parts(&pattern[1..], &path[1..])
            }
            else {
                false
            }
        }
        (Some(_), None) => false,
    }
}

fn glob_segment_match(pattern: &str, segment: &str) -> bool {
    if pattern == "*" {
        return !segment.is_empty();
    }
    if !pattern.contains('*') {
        return pattern == segment;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut remainder = segment;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if index == 0 {
            if !remainder.starts_with(part) {
                return false;
            }
            remainder = &remainder[part.len()..];
        }
        else if index == parts.len() - 1 {
            if !remainder.ends_with(part) {
                return false;
            }
        }
        else if let Some(found) = remainder.find(part) {
            remainder = &remainder[found + part.len()..];
        }
        else {
            return false;
        }
    }
    true
}

/// 包外壳（envelope）schema 版本。
const ENVELOPE_SCHEMA: u32 = 1;

/// 把单个发布产物归类为 envelope 声明。
///
/// `.odin` 是执行字节码身份；`.nyar` 只能作为程序集运输身份出现，出现在可执行入口时
/// 必须结构化拒绝，不能当成 `.odin` 的别名发布。
fn artifact_envelope_entry(relative: &str) -> Result<serde_json::Value> {
    let extension = Path::new(relative).extension().and_then(|value| value.to_str()).unwrap_or_default().to_ascii_lowercase();
    let (kind, format, format_version) = match extension.as_str() {
        "odin" => ("execution", "odin", Some(acorn_odin::Odin_VERSION)),
        "wasm" => ("execution", "wasm", None),
        "mjs" | "js" => ("host-module", "ecmascript", None),
        "nyar" => {
            return Err(PublishError::message(format!(
                "拒绝打包：`{relative}` 是语义程序集（Assembly），不能作为可执行入口发布；执行入口必须是 `.odin` 或宿主模块"
            )));
        }
        _ => ("data", "raw", None),
    };
    let mut entry = serde_json::json!({ "path": relative, "kind": kind, "format": format });
    if let Some(version) = format_version {
        entry["format_version"] = serde_json::json!(version);
    }
    Ok(entry)
}

fn build_envelope(bins: &[RegistryBinEntry]) -> Result<serde_json::Value> {
    let artifacts = bins.iter().map(|entry| artifact_envelope_entry(&entry.relative_path)).collect::<Result<Vec<_>>>()?;
    Ok(serde_json::json!({ "schema": ENVELOPE_SCHEMA, "artifacts": artifacts }))
}

fn build_package_json(meta: &PackMeta, bins: &[RegistryBinEntry], registry: &str) -> Result<String> {
    let mut bin_map = BTreeMap::new();
    let mut exports = BTreeMap::new();
    for entry in bins {
        let path = format!("./{}", entry.relative_path);
        bin_map.insert(entry.bin_name.clone(), path.clone());
        exports.insert(format!("./{}", entry.bin_name), path.clone());
    }
    if let Some(first) = bins.first() {
        exports.insert(".".to_string(), format!("./{}", first.relative_path));
    }

    let mut doc = serde_json::json!({
        "name": meta.name,
        "version": meta.version,
        "description": meta.description,
        "license": meta.license.clone().unwrap_or_else(|| "UNLICENSED".to_string()),
        "type": "module",
        "bin": bin_map,
        "exports": exports,
    });

    if registry.eq_ignore_ascii_case("npm") {
        doc["engines"] = serde_json::json!({ "node": ">=18" });
    }
    doc["nyar"] = build_envelope(bins)?;

    Ok(serde_json::to_string_pretty(&doc).unwrap_or_else(|_| doc.to_string()))
}

fn build_jsr_json(meta: &PackMeta, bins: &[RegistryBinEntry]) -> String {
    let mut exports = BTreeMap::new();
    for entry in bins {
        exports.insert(format!("./{}", entry.bin_name), format!("./{}", entry.relative_path));
    }
    let doc = serde_json::json!({
        "name": meta.name,
        "version": meta.version,
        "exports": exports,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| doc.to_string())
}

fn registry_bin_entries(contracts: &[RunContract], artifact_dir: &Path, layout: ProjectLayout) -> Result<Vec<RegistryBinEntry>> {
    let mut entries = Vec::new();
    for contract in contracts {
        let path = resolve_contract_artifact(artifact_dir, contract, layout)?;
        let relative = path.strip_prefix(artifact_dir).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        if !is_publishable_artifact(&relative) {
            return Err(PublishError::message(format!(
                "run contract `{}` 指向不可发布的产物 `{relative}`",
                contract.logical_entry
            )));
        }
        entries.push(RegistryBinEntry { bin_name: bin_name_from_logical(&contract.logical_entry), relative_path: relative });
    }
    Ok(entries)
}

/// 从 `logical_entry` 提取 bin 短名（取 `::` 分隔后的最后一段）。
pub fn bin_name_from_logical(logical_entry: &str) -> String {
    let normalized = logical_entry.replace('\u{2237}', "::");
    normalized.rsplit("::").next().unwrap_or(&normalized).trim().to_string()
}

fn paired_wasm_path(mjs_relative: &str) -> Option<String> {
    let path = Path::new(mjs_relative);
    let stem = path.file_stem()?.to_str()?;
    let parent = path.parent().map(|value| value.to_string_lossy().replace('\\', "/")).filter(|value| !value.is_empty());
    let wasm_name = format!("{stem}.wasm");
    Some(match parent {
        Some(prefix) => format!("{prefix}/{wasm_name}"),
        None => wasm_name,
    })
}

fn is_publishable_artifact(relative: &str) -> bool {
    let name = Path::new(relative).file_name().and_then(|value| value.to_str()).unwrap_or(relative);
    if name.starts_with("run-contract") {
        return false;
    }
    if name.starts_with("wasmtime-") {
        return false;
    }
    let lower = relative.to_ascii_lowercase();
    !(lower.ends_with(".msil") || lower.contains("/wasmtime-"))
}

fn list_artifact_files(artifact_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_files_flat(artifact_dir, artifact_dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_files_flat(root: &Path, current: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|value| value.to_str()).unwrap_or_default();
            if name.starts_with("wasmtime") {
                continue;
            }
            collect_files_flat(root, &path, files)?;
        }
        else if path.is_file() {
            let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            if is_publishable_artifact(&relative) {
                files.push(path);
            }
        }
    }
    Ok(())
}

fn resolve_contract_artifact(artifact_dir: &Path, contract: &RunContract, layout: ProjectLayout) -> Result<PathBuf> {
    let physical_entry = contract.physical_entry.trim();
    if physical_entry.is_empty() {
        return Err(PublishError::message(format!("run contract `{}` 缺少 physical_entry", contract.logical_entry)));
    }
    for candidate in (layout.entry_aliases)(physical_entry).iter().copied().chain(std::iter::once(physical_entry)) {
        let direct = artifact_dir.join(candidate);
        if direct.is_file() {
            return Ok(direct);
        }
    }
    Err(PublishError::message(format!(
        "run contract `{}` 指向的产物 `{physical_entry}` 不存在于 {}",
        contract.logical_entry,
        artifact_dir.display()
    )))
}

#[derive(Debug, Clone, Deserialize)]
struct ExecutionManifest {
    #[serde(default)]
    run_contracts: Vec<RunContract>,
}

#[derive(Debug, Clone, Deserialize)]
struct RunContract {
    logical_entry: String,
    physical_entry: String,
}

fn load_run_contracts(artifact_dir: &Path) -> Result<Vec<RunContract>> {
    let contracts_path = artifact_dir.join("run-contracts.txt");
    if !contracts_path.is_file() {
        return Ok(Vec::new());
    }
    let source = std::fs::read_to_string(&contracts_path)?;
    let manifest: ExecutionManifest = oak_von::from_str(&source).map_err(|error| PublishError::Resolve(PackageError::von(error)))?;
    Ok(manifest.run_contracts)
}

fn collect_files(package_directory: &Path, ignore: &PackageIgnore) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    visit(package_directory, package_directory, ignore, &mut files)?;
    files.sort();
    Ok(files)
}

fn visit(root: &Path, current: &Path, ignore: &PackageIgnore, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        if ignore.is_ignored(&relative) {
            continue;
        }
        if path.is_dir() {
            visit(root, &path, ignore, files)?;
        }
        else if path.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

/// 把 gzip tar 字节解到 `target_directory`。
pub fn unpack(tarball_data: &[u8], target_directory: &Path) -> Result<usize> {
    package_registry::extract_tarball(tarball_data, target_directory)?;
    Ok(std::fs::read_dir(target_directory).map(|entries| entries.count()).unwrap_or(0))
}
