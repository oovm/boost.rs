//! 包管理器编排（`package.manager`）。
//!
//! 把清单 / lock（`package.resolve`）、渠道（`package.registry`）、缓存（`package.cache`）
//! 与打包发布（`package.publish`）组合成安装、更新、发布、登录与审计流程。
//!
//! 宿主注入两件事：注册表适配器工厂（自有渠道与代理在这里接入）与凭据策略
//! （官方 CLI 存储的发现与写回不在包机制内）。

#![deny(missing_docs)]

mod auth;
mod credentials;
mod error;
mod manager;
mod registry_sources;
pub mod security;

use std::sync::Arc;

pub use auth::{LoginResult, VendorAuthStore, VendorManager};
pub use credentials::{CredentialProvider, DiscoveredCredential, NoCredentials, RegistryCredentialId};
pub use error::{ManagerError, Result};
pub use manager::{PackageInfo, PackageManager};
pub use package_resolve::{
    DependencyBucket, DependencyNode, DependencySpec, EntryAliasFn, LockEntry, LockFile, PackageManifest, ProjectLayout, ProjectMode,
    PublishConfig, PublishTarget, SemanticVersion, WorkspaceManifest, YearlyVersion, is_registry_publish_type,
    satisfies_version_constraint,
};
pub use package_publish::{
    PackMeta, PackResult, PackageIgnore, PackagePublisher, RegistryBinEntry, RegistryPackOptions, ResolvedPublishArtifact,
    ScriptResult, ScriptRunner, bin_name_from_logical, glob_match, pack, pack_registry_artifact, resolve_publish_artifact, unpack,
};
pub use registry_sources::{RegistryFactory, RegistrySourceManager, default_registry_factory};
pub use security::{
    LicenseInfo, SecurityAudit, SecurityAuditResult, VulnerabilityReport, check_license, is_license_compatible, is_license_restricted,
};

/// 包管理器需要的宿主服务。
#[derive(Clone)]
pub struct ManagerServices {
    /// 注册表适配器工厂。
    pub registries: RegistryFactory,
    /// 凭据策略。
    pub credentials: Arc<dyn CredentialProvider>,
}

impl Default for ManagerServices {
    fn default() -> Self {
        Self { registries: default_registry_factory(), credentials: Arc::new(NoCredentials) }
    }
}

impl ManagerServices {
    /// 以显式工厂与凭据策略构造。
    pub fn new(registries: RegistryFactory, credentials: Arc<dyn CredentialProvider>) -> Self {
        Self { registries, credentials }
    }
}
