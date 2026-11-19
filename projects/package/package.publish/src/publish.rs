use std::{collections::HashMap, path::Path, process::Command, sync::Arc};

use package_registry::{PublishOptions, PublishResult, Registry, VersionBump};
use package_resolve::{PackageManifest, ProjectLayout, SemanticVersion};

use crate::{
    PublishError, Result, ScriptRunner,
    pack::{self, PackMeta, PackResult, RegistryPackOptions},
};

/// 发布流水线：版本递增 → Git 检查 → 打包 → 上传 → 可选打 tag。
pub struct PackagePublisher {
    registries: HashMap<String, Arc<dyn Registry>>,
    layout: ProjectLayout,
}

impl PackagePublisher {
    /// 绑定注册表适配器表与工程布局。
    pub fn new(registries: HashMap<String, Arc<dyn Registry>>, layout: ProjectLayout) -> Self {
        Self { registries, layout }
    }

    /// 执行完整发布：版本递增 → Git 检查 → 打包 → 上传 → 可选打 tag。
    pub fn publish(&self, mut options: PublishOptions) -> Result<PublishResult> {
        if options.package_name.trim().is_empty() {
            return Err(PublishError::message("包名不能为空"));
        }
        if options.package_path.trim().is_empty() {
            return Err(PublishError::message("包路径不能为空"));
        }
        let package_path = Path::new(&options.package_path);
        if !package_path.is_dir() {
            return Err(PublishError::message(format!("包目录不存在: {}", options.package_path)));
        }

        if let Some(bump) = options.bump {
            options.version = bump_version(package_path, &options.version, bump, self.layout)?;
        }
        if options.version.trim().is_empty() {
            return Err(PublishError::message("版本号不能为空"));
        }

        if !options.skip_git_check
            && let Some(message) = git_dirty_message(package_path)
        {
            let _ = message;
        }

        if options.run_pre_publish_script {
            run_pre_publish_script(package_path, self.layout)?;
        }

        let pack_meta = PackMeta {
            name: options.package_name.clone(),
            version: options.version.clone(),
            description: options.description.clone(),
            license: options.license.clone(),
        };
        let pack_result = if let Some(artifact_dir) = options.artifact_dir.as_deref() {
            pack::pack_registry_artifact(&RegistryPackOptions {
                artifact_dir: Path::new(artifact_dir).to_path_buf(),
                package_root: package_path.to_path_buf(),
                registry: options.registry_name.clone(),
                meta: pack_meta,
                include_files: options.include_files.clone(),
                flat_layout: options.flat_layout,
                layout: self.layout,
            })?
        }
        else {
            pack::pack(package_path, &pack_meta, self.layout)?
        };

        if options.dry_run {
            return Ok(PublishResult {
                success: true,
                package_name: options.package_name,
                version: options.version,
                message: "dry-run: 跳过实际上传".to_string(),
                published_url: None,
                dry_run: true,
                sha256: Some(pack_result.sha256),
                size: Some(pack_result.size),
                file_count: Some(pack_result.file_count),
                official_tool_required: false,
            });
        }

        let registry = self
            .registries
            .get(&options.registry_name)
            .ok_or_else(|| PublishError::message(format!("注册器 {} 未找到", options.registry_name)))?;
        let mut result = registry.publish_package(&options, &pack_result.tarball_data)?;
        result.sha256 = Some(pack_result.sha256);
        result.size = Some(pack_result.size);
        result.file_count = Some(pack_result.file_count);

        if result.success && options.create_git_tag {
            let prefix = options.git_tag_prefix.as_deref().unwrap_or("v");
            let _ = create_git_tag(package_path, &options.version, prefix);
        }
        Ok(result)
    }

    /// 仅打包不上传，用于本地验证 tarball 布局与摘要。
    pub fn pack_only(&self, package_path: &Path, meta: &PackMeta) -> Result<PackResult> {
        pack::pack(package_path, meta, self.layout)
    }
}

fn bump_version(package_path: &Path, current: &str, bump: VersionBump, layout: ProjectLayout) -> Result<String> {
    let mut manifest = PackageManifest::load(package_path, layout)?;
    let base = if current.trim().is_empty() { manifest.version.clone() } else { current.to_string() };
    let next = SemanticVersion::parse(&base)?.bump(bump).to_string();
    manifest.version = next.clone();
    manifest.save(package_path, layout)?;
    Ok(next)
}

fn run_pre_publish_script(package_path: &Path, layout: ProjectLayout) -> Result<()> {
    let manifest = PackageManifest::load(package_path, layout)?;
    let Some(command) = manifest
        .scripts
        .get("prePublish")
        .cloned()
        .or_else(|| manifest.hooks.get("pre_publish").cloned())
        .or_else(|| manifest.hooks.get("prePublish").cloned())
    else {
        return Ok(());
    };
    let result = ScriptRunner::new(package_path).run("prePublish", &command)?;
    if !result.success {
        return Err(PublishError::message(format!("prePublish failed: {}", result.stderr)));
    }
    Ok(())
}

fn git_dirty_message(package_path: &Path) -> Option<String> {
    let output = Command::new("git").args(["status", "--porcelain"]).current_dir(package_path).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.lines().take(5).collect::<Vec<_>>().join("; ")) }
}

fn create_git_tag(package_path: &Path, version: &str, prefix: &str) -> Result<String> {
    let tag = format!("{prefix}{version}");
    let status = Command::new("git").args(["tag", &tag]).current_dir(package_path).status()?;
    if status.success() { Ok(tag) } else { Err(PublishError::message(format!("git tag {tag} failed"))) }
}
