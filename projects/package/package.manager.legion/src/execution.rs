//! execution manifest 摘要与可信复用判定。
//!
//! 唯一的权威校验核心是 `crate::legion::cmds::run::ExecutionManifest`；本模块只做
//! 宿主友好的字段投影，不另造一套新鲜度规则。

use std::path::{Path, PathBuf};

use crate::legion::{
    cmds::{
        error::LegionResult,
        run::{
            ExecutionManifest, LibraryExportIndexEntry, ManifestVerdict, ManifestVerificationContext, ManifestVerificationLevel,
            collect_execution_input_digests, expected_resolver_compile_evidence,
        },
    },
    resolve_platform_only_target,
    workspace::{
        manifest::ProjectArtifactKind,
        planner::{BuildRequest, WorkspaceResolver},
    },
};

/// execution manifest 的宿主友好摘要。
#[derive(Debug, Clone)]
pub struct ManifestView {
    /// schema 版本。
    pub schema_version: u32,
    /// canonical target 字符串。
    pub target: String,
    /// 产物种类：`binary` / `library`。
    pub artifact_kind: String,
    /// 库导出索引（逻辑名 + 物理产物）；非库产物为空。
    pub library_exports: Vec<LibraryExportIndexEntry>,
    /// 语义闭包哈希。
    pub semantic_closure_hash: Option<String>,
    /// 规范表面哈希。
    pub canonical_surface_hash: Option<String>,
    /// host provider 绑定哈希。
    pub host_provider_binding_hash: Option<String>,
    /// 首个 run contract 物理入口。
    pub primary_physical_entry: Option<String>,
    /// 首个 run contract 逻辑入口。
    pub primary_logical_entry: Option<String>,
    /// 产出该构建的编译器构建身份；旧构建缺少该段时为 `None`。
    pub compiler_build_identity: Option<String>,
}

/// 产物新鲜度检查结果。
#[derive(Debug, Clone)]
pub struct Freshness {
    /// 是否可信复用；只有 `level == "fully-verified"` 才为 `true`。
    pub fresh: bool,
    /// 失败原因。
    pub reason: Option<String>,
    /// 权威校验级别：`fully-verified` / `artifact-self-consistent` / `stale`。
    pub level: String,
}

/// 读取并投影 `run-contracts.txt`；缺失或解析失败都返回 `None`。
#[must_use]
pub fn read_manifest(output_dir: &Path) -> Option<ManifestView> {
    let manifest = ExecutionManifest::read_from_output_dir(output_dir).ok().flatten()?;
    let contract = manifest.run_contracts.first();
    let evidence = manifest.compile_evidence.as_ref();
    Some(ManifestView {
        schema_version: manifest.schema_version,
        target: manifest.target,
        artifact_kind: match manifest.artifact_kind {
            ProjectArtifactKind::Binary => "binary".to_string(),
            ProjectArtifactKind::Library => "library".to_string(),
        },
        library_exports: manifest.library_exports,
        semantic_closure_hash: evidence.map(|evidence| evidence.semantic_closure_hash.clone()),
        canonical_surface_hash: evidence.map(|evidence| evidence.canonical_surface_hash.clone()),
        host_provider_binding_hash: evidence.map(|evidence| evidence.host_provider_binding_hash.clone()),
        primary_physical_entry: contract.map(|contract| contract.physical_entry.clone()),
        primary_logical_entry: contract.map(|contract| contract.logical_entry.clone()),
        compiler_build_identity: crate::legion::cache::compiler_build_id_from_fingerprint(&manifest.toolchain_fingerprint).map(str::to_string),
    })
}

/// 本进程内嵌的编译器构建身份。
#[must_use]
pub fn compiler_build_identity() -> String {
    crate::legion::cache::compiler_build_id().to_string()
}

/// 本进程的完整 toolchain fingerprint。
#[must_use]
pub fn toolchain_fingerprint() -> String {
    crate::legion::cache::toolchain_fingerprint()
}

const FRESHNESS_LEVEL_FULLY: &str = "fully-verified";
const FRESHNESS_LEVEL_SELF_CONSISTENT: &str = "artifact-self-consistent";
const FRESHNESS_LEVEL_STALE: &str = "stale";
const EXECUTION_MANIFEST_FILE: &str = "run-contracts.txt";

fn stale(reason: String) -> Freshness {
    Freshness { fresh: false, reason: Some(reason), level: FRESHNESS_LEVEL_STALE.into() }
}

fn verdict_to_freshness(verdict: LegionResult<ManifestVerdict>) -> Freshness {
    match verdict {
        Ok(ManifestVerdict::Verified(ManifestVerificationLevel::FullyVerified)) => {
            Freshness { fresh: true, reason: None, level: FRESHNESS_LEVEL_FULLY.into() }
        }
        Ok(ManifestVerdict::Verified(ManifestVerificationLevel::ArtifactSelfConsistent)) => Freshness {
            fresh: false,
            reason: Some(
                "artifact self-consistent only: inputs not verified against current workspace; pass projectDir for formal reuse".into(),
            ),
            level: FRESHNESS_LEVEL_SELF_CONSISTENT.into(),
        },
        Ok(ManifestVerdict::Stale(reason)) => stale(reason),
        Err(error) => stale(format!("manifest verification failed: {error}")),
    }
}

/// 判断输出目录中的 execution manifest 是否可信复用。
///
/// 提供 `project_dir` 时重算当前 workspace 输入摘要与 resolver compile evidence，
/// 可达到 `fully-verified`；不提供时最高只能达到 `artifact-self-consistent`。
#[must_use]
pub fn check_artifact_fresh(output_dir: &Path, expected_target: &str, project_dir: Option<&Path>) -> Freshness {
    let manifest_path = output_dir.join(EXECUTION_MANIFEST_FILE);
    if !manifest_path.is_file() {
        return stale("missing run-contracts.txt".into());
    }
    let manifest = match ExecutionManifest::read_from_output_dir(output_dir) {
        Ok(Some(manifest)) => manifest,
        Ok(None) | Err(_) => return stale("failed to parse run-contracts.txt".into()),
    };
    if manifest.target != expected_target {
        return stale(format!("manifest target `{}` != expected `{}`", manifest.target, expected_target));
    }
    let verdict = match project_dir {
        Some(dir) => verify_against_project(&manifest, output_dir, expected_target, dir),
        None => manifest.verify_against_output_dir(output_dir),
    };
    verdict_to_freshness(verdict)
}

fn verify_against_project(
    manifest: &ExecutionManifest,
    output_dir: &Path,
    expected_target: &str,
    project_dir: &Path,
) -> LegionResult<ManifestVerdict> {
    let target = resolve_platform_only_target(expected_target)
        .map_err(|error| crate::legion::LegionError::msg(format!("invalid expected target `{expected_target}`: {error}")))?;
    let workspace = WorkspaceResolver::discover_for_project(project_dir)
        .map_err(|error| crate::legion::LegionError::msg(format!("workspace discovery failed for `{}`: {error}", project_dir.display())))?;
    let request = BuildRequest {
        project_dir: project_dir.to_path_buf(),
        resolved: target,
        output_dir: Some(PathBuf::from(output_dir)),
        ..Default::default()
    };
    let plan = workspace
        .build_plan(&request)
        .map_err(|error| crate::legion::LegionError::msg(format!("build plan failed for `{}`: {error}", project_dir.display())))?;
    let inputs = collect_execution_input_digests(&plan)?;
    let expected = expected_resolver_compile_evidence(&plan)?;
    let plan_target = plan.project.build_target.target.to_string();
    let context = ManifestVerificationContext {
        expected_project: Some((plan.project.name.as_str(), plan_target.as_str(), plan.project.artifact_kind)),
        current_inputs: Some(&inputs),
        expected_evidence: Some(&expected),
    };
    manifest.verify(output_dir, &context)
}
