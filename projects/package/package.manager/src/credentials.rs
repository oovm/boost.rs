//! 凭据概念与宿主注入接口。
//!
//! 凭据发现与写回属于宿主策略：读 `.npmrc`、Deno 配置、NuGet.Config 一类官方 CLI 存储
//! 不是包机制，因此本 crate 只定义接口，由调用方实现并注入。

use std::path::{Path, PathBuf};

use crate::{ManagerError, Result};

/// 从官方工具存储发现的令牌。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCredential {
    /// Bearer 或 API 令牌值。
    pub token: String,
    /// 可读来源（例如 `.npmrc` 或环境变量名）。
    pub source: String,
}

/// 宿主注入的凭据策略。
pub trait CredentialProvider: Send + Sync {
    /// 该注册表是否只使用外部 CLI 凭据存储（不写入 `auth.von`）。
    fn uses_external_store(&self, registry: &str) -> bool;

    /// 从官方 CLI 配置发现令牌。
    fn discover(&self, registry: &str, endpoint: Option<&str>, project_dir: Option<&Path>) -> Option<DiscoveredCredential>;

    /// 把已验证令牌写回对应的官方 CLI 存储。
    fn sync_to_official(&self, registry: &str, endpoint: Option<&str>, token: &str) -> Result<Option<PathBuf>>;
}

/// 不提供任何外部凭据的默认策略。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCredentials;

impl CredentialProvider for NoCredentials {
    fn uses_external_store(&self, _registry: &str) -> bool {
        false
    }

    fn discover(&self, _registry: &str, _endpoint: Option<&str>, _project_dir: Option<&Path>) -> Option<DiscoveredCredential> {
        None
    }

    fn sync_to_official(&self, _registry: &str, _endpoint: Option<&str>, _token: &str) -> Result<Option<PathBuf>> {
        Ok(None)
    }
}

/// 由注册表 id 与 endpoint 组成的轻量凭据查询键。
#[derive(Debug, Clone, Copy)]
pub struct RegistryCredentialId<'a> {
    /// 注册表逻辑名称。
    pub id: &'a str,
    /// 覆盖默认 endpoint 时的 API 根地址。
    pub endpoint: Option<&'a str>,
}

impl RegistryCredentialId<'_> {
    /// 按本键发现凭据。
    pub fn discover(&self, provider: &dyn CredentialProvider, project_dir: Option<&Path>) -> Option<DiscoveredCredential> {
        provider.discover(self.id, self.endpoint, project_dir)
    }
}

/// 供宿主复用的凭据错误包装。
pub fn credential_error(message: impl Into<String>) -> ManagerError {
    ManagerError::message(message)
}
