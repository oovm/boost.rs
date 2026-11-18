use std::path::Path;

use crate::{Package, PublishOptions, PublishResult, RegistryError, TokenVerifyResult};

/// 注册表适配器合同。
///
/// 实现只拥有该渠道的请求与响应规则：身份与版本元数据、制品位置、发布与令牌校验。
/// 版本求解、缓存落盘与发布计划不在本 trait 内。
pub trait Registry: Send + Sync {
    /// 渠道逻辑名（`npm`、`jsr` 等）。
    fn name(&self) -> &str;

    /// API 根地址（不带尾部斜杠）。
    fn endpoint(&self) -> &str;

    /// 覆盖 API 根地址。
    fn set_endpoint(&mut self, endpoint: &str);

    /// 读取包元数据（`version = "latest"` 解析最新版本）。
    fn get_package(&self, package_name: &str, version: &str) -> Result<Package, RegistryError>;

    /// 按关键字搜索包。
    fn search_packages(&self, query: &str) -> Result<Vec<Package>, RegistryError>;

    /// 把已打包的字节发布到该渠道。
    fn publish_package(&self, options: &PublishOptions, tarball_data: &[u8]) -> Result<PublishResult, RegistryError>;

    /// 下载并解出包内容到 `target_directory`。
    fn download_package(&self, package: &Package, target_directory: &Path) -> Result<String, RegistryError>;

    /// 列出某个包的全部可用版本。
    fn get_package_versions(&self, package_name: &str) -> Result<Vec<String>, RegistryError>;

    /// 校验认证令牌。
    fn verify_token(&self, token: &str) -> Result<TokenVerifyResult, RegistryError>;

    /// `package_name@version` 是否存在。
    fn package_exists(&self, package_name: &str, version: &str) -> Result<bool, RegistryError> {
        Ok(self.get_package_versions(package_name)?.iter().any(|item| item == version))
    }

    /// 解析最新稳定版本；不存在时返回 `None`。
    fn get_latest_version(&self, package_name: &str) -> Result<Option<String>, RegistryError> {
        match self.get_package(package_name, "latest") {
            Ok(package) => Ok(Some(package.version)),
            Err(RegistryError::NotFound(_)) => Ok(None),
            Err(RegistryError::Status { status: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }
}
