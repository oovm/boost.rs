use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use package_registry::{HttpClient, OsvClient, Package, ProxyConfig, RetryConfig};
use package_resolve::ProjectLayout;
use serde::{Deserialize, Serialize};

use crate::{ManagerError, Result};

/// 锁定包的漏洞报告。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VulnerabilityReport {
    /// 受影响的包名。
    pub package_name: String,
    /// 受影响的版本。
    pub version: String,
    /// OSV / CVE 等漏洞编号。
    pub vulnerability_id: Option<String>,
    /// 漏洞摘要标题。
    pub title: String,
    /// 严重级别。
    pub severity: String,
    /// 建议升级到的修复版本。
    pub fixed_version: Option<String>,
    /// 参考链接。
    pub url: Option<String>,
}

/// 许可证审计条目。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LicenseInfo {
    /// 被审计的包名。
    pub package_name: String,
    /// 被审计的版本。
    pub version: String,
    /// 声明的 SPDX 许可证。
    pub license: String,
    /// 是否在允许列表内。
    pub is_compatible: bool,
    /// 是否属于 copyleft / 商业限制许可证。
    pub is_restricted: bool,
}

/// 合并后的安全审计结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecurityAuditResult {
    /// OSV 查询得到的漏洞条目。
    pub vulnerabilities: Vec<VulnerabilityReport>,
    /// 许可证合规结果。
    pub licenses: Vec<LicenseInfo>,
}

impl SecurityAuditResult {
    /// 是否发现至少一条漏洞报告。
    pub fn has_vulnerabilities(&self) -> bool {
        !self.vulnerabilities.is_empty()
    }

    /// 是否存在不兼容或受限许可证。
    pub fn has_license_issues(&self) -> bool {
        self.licenses.iter().any(|license| !license.is_compatible || license.is_restricted)
    }

    /// 合并另一份审计结果。
    pub fn merge(&mut self, other: SecurityAuditResult) {
        self.vulnerabilities.extend(other.vulnerabilities);
        self.licenses.extend(other.licenses);
    }

    /// 按 `name@version` 汇总建议升级到的最高修复版本。
    pub fn fix_suggestions(&self) -> BTreeMap<String, String> {
        let mut suggestions = BTreeMap::new();
        for report in &self.vulnerabilities {
            let Some(fixed) = &report.fixed_version
            else {
                continue;
            };
            let key = format!("{}@{}", report.package_name, report.version);
            suggestions
                .entry(key)
                .and_modify(|current: &mut String| {
                    if fixed.as_str() > current.as_str() {
                        *current = fixed.clone();
                    }
                })
                .or_insert_with(|| fixed.clone());
        }
        suggestions
    }
}

/// OSV + 许可证策略引擎。
pub struct SecurityAudit {
    cache_path: PathBuf,
    cache: BTreeMap<String, Vec<VulnerabilityReport>>,
    skip_network: bool,
    osv: OsvClient,
}

impl SecurityAudit {
    /// 使用工程布局的缓存目录打开。
    pub fn open_with_layout(layout: ProjectLayout) -> Result<Self> {
        let cache_dir = layout.cache_dir();
        std::fs::create_dir_all(&cache_dir)?;
        Self::with_cache_path(cache_dir.join("vulnerability-cache.json"))
    }

    /// 使用自定义 OSV 结果缓存文件路径。
    pub fn with_cache_path(cache_path: impl AsRef<Path>) -> Result<Self> {
        let cache_path = cache_path.as_ref().to_path_buf();
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let cache = load_cache(&cache_path);
        Ok(Self { cache_path, cache, skip_network: false, osv: osv_client()? })
    }

    /// 关闭对外 OSV 调用（离网 / 单元测试）。
    pub fn offline(mut self) -> Self {
        self.skip_network = true;
        self
    }

    /// 对单个包执行 OSV 扫描与许可证检查。
    pub fn audit_package(&mut self, package: &Package, registry: &str) -> Result<SecurityAuditResult> {
        Ok(SecurityAuditResult { vulnerabilities: self.scan_vulnerabilities(package, registry)?, licenses: vec![check_license(package)] })
    }

    /// 批量审计多个 `(Package, registry)` 并合并结果。
    pub fn audit_dependencies(&mut self, packages: &[(Package, String)]) -> Result<SecurityAuditResult> {
        let mut result = SecurityAuditResult::default();
        for (package, registry) in packages {
            result.merge(self.audit_package(package, registry)?);
        }
        Ok(result)
    }

    /// 许可证是否在允许列表内。
    pub fn is_license_compatible(license: &str) -> bool {
        crate::security::is_license_compatible(license)
    }

    /// 许可证是否属于受限类别。
    pub fn is_license_restricted(license: &str) -> bool {
        crate::security::is_license_restricted(license)
    }

    fn scan_vulnerabilities(&mut self, package: &Package, registry: &str) -> Result<Vec<VulnerabilityReport>> {
        let key = format!("{}@{}", package.name, package.version);
        if let Some(cached) = self.cache.get(&key) {
            return Ok(cached.clone());
        }

        let Some(ecosystem) = package_registry::osv_ecosystem(registry)
        else {
            return Ok(Vec::new());
        };
        if self.skip_network {
            return Ok(Vec::new());
        }

        let reports = match self.osv.query(&package.name, &package.version, ecosystem) {
            Ok(advisories) => advisories
                .into_iter()
                .map(|advisory| VulnerabilityReport {
                    package_name: package.name.clone(),
                    version: package.version.clone(),
                    vulnerability_id: advisory.id,
                    title: advisory.summary,
                    severity: advisory.severity,
                    fixed_version: advisory.fixed_version,
                    url: advisory.url,
                })
                .collect::<Vec<_>>(),
            Err(error) => {
                eprintln!("warning: OSV query failed for {key}: {error}");
                Vec::new()
            }
        };
        self.cache.insert(key, reports.clone());
        let _ = save_cache(&self.cache_path, &self.cache);
        Ok(reports)
    }
}

/// OSV 查询使用宿主代理策略：这里显式读取代理环境变量。
fn osv_client() -> Result<OsvClient> {
    let retry = RetryConfig::default();
    let http = match ProxyConfig::from_env().map_err(|error| ManagerError::message(error.to_string()))? {
        Some(proxy) => HttpClient::with_proxy(retry, proxy),
        None => HttpClient::new(retry).map_err(|error| ManagerError::message(error.to_string()))?,
    };
    Ok(OsvClient::with_http(http))
}

/// 计算包的许可证兼容 / 受限信息。
pub fn check_license(package: &Package) -> LicenseInfo {
    let license = if package.license.trim().is_empty() { "unknown".to_string() } else { package.license.clone() };
    let restricted = is_license_restricted(&license);
    let compatible = is_license_compatible(&license) && !restricted;
    LicenseInfo {
        package_name: package.name.clone(),
        version: package.version.clone(),
        license,
        is_compatible: compatible,
        is_restricted: restricted,
    }
}

/// 许可证是否在允许列表内。
pub fn is_license_compatible(license: &str) -> bool {
    if license.eq_ignore_ascii_case("unknown") {
        return false;
    }
    COMPATIBLE.iter().any(|item| license.eq_ignore_ascii_case(item))
}

/// 许可证是否属于受限类别。
pub fn is_license_restricted(license: &str) -> bool {
    RESTRICTED.iter().any(|item| license.eq_ignore_ascii_case(item))
}

/// 允许列表：调用方可用 [`SecurityAudit`] 的结果自行决定更严格策略。
const COMPATIBLE: &[&str] = &["MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "0BSD", "ISC", "Unlicense", "CC0-1.0", "WTFPL", "Zlib"];

const RESTRICTED: &[&str] =
    &["GPL-2.0-only", "GPL-2.0-or-later", "GPL-3.0-only", "GPL-3.0-or-later", "AGPL-3.0-only", "AGPL-3.0-or-later", "SSPL-1.0", "BUSL-1.1"];

fn load_cache(path: &Path) -> BTreeMap<String, Vec<VulnerabilityReport>> {
    if !path.is_file() {
        return BTreeMap::new();
    }
    std::fs::read_to_string(path).ok().and_then(|source| serde_json::from_str(&source).ok()).unwrap_or_default()
}

fn save_cache(path: &Path, cache: &BTreeMap<String, Vec<VulnerabilityReport>>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cache).map_err(|error| ManagerError::message(error.to_string()))?;
    std::fs::write(path, json)?;
    Ok(())
}
