//! 制品漏洞公告查询（OSV）。
//!
//! OSV 是按包身份与版本查询漏洞公告的公共数据库，属于包生态的通用机制，因此与渠道合同
//! 放在同一 crate。许可证合规策略不在本模块：允许/受限许可证清单是产品策略。

use serde::Deserialize;

use crate::{HttpClient, RegistryError, RetryConfig};

/// OSV 批量查询接口。
pub const OSV_ENDPOINT: &str = "https://api.osv.dev/v1/query";

/// 一条漏洞公告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advisory {
    /// 公告 id（CVE 可用时优先，否则为 OSV id）。
    pub id: Option<String>,
    /// 公告标题摘要。
    pub summary: String,
    /// 严重级别；OSV 未给出时为 `unknown`。
    pub severity: String,
    /// 建议升级到的修复版本。
    pub fixed_version: Option<String>,
    /// 第一条参考链接。
    pub url: Option<String>,
}

/// 按渠道名映射 OSV 生态名。
///
/// 只覆盖本 crate 认识的第三方渠道；自有渠道由调用方自行映射或返回 `None`。
pub fn osv_ecosystem(registry: &str) -> Option<&'static str> {
    match registry.trim().to_ascii_lowercase().as_str() {
        "npm" | "jsr" => Some("npm"),
        "nuget" => Some("NuGet"),
        "maven" => Some("Maven"),
        "pypi" | "conda" => Some("PyPI"),
        "crates" | "crates-io" => Some("crates.io"),
        _ => None,
    }
}

/// OSV 查询客户端。
#[derive(Debug, Clone)]
pub struct OsvClient {
    endpoint: String,
    http: HttpClient,
}

impl OsvClient {
    /// 使用默认 OSV 地址与默认重试策略。
    pub fn new() -> Result<Self, RegistryError> {
        Ok(Self { endpoint: OSV_ENDPOINT.to_string(), http: HttpClient::new(RetryConfig::default())? })
    }

    /// 用调用方配置好的客户端连接（代理、注入传输由调用方决定）。
    pub fn with_http(http: HttpClient) -> Self {
        Self { endpoint: OSV_ENDPOINT.to_string(), http }
    }

    /// 覆盖 OSV 地址（自建镜像或验收桩）。
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into().trim_end_matches('/').to_string();
        self
    }

    /// 查询 `package@version` 在给定生态下的公告。
    pub fn query(&self, package: &str, version: &str, ecosystem: &str) -> Result<Vec<Advisory>, RegistryError> {
        let payload = serde_json::json!({
            "package": { "name": package, "ecosystem": ecosystem },
            "version": version,
        });
        let body = serde_json::to_vec(&payload)?;
        let response = self.http.post_body(&self.endpoint, &body, "application/json")?;
        if !response.is_success() {
            return Err(RegistryError::status(response.status, response.text()));
        }
        let parsed: OsvResponse = response.json()?;
        Ok(parsed.vulns.unwrap_or_default().into_iter().map(Advisory::from).collect())
    }
}

impl From<OsvVuln> for Advisory {
    fn from(vuln: OsvVuln) -> Self {
        let id = vuln.id.or_else(|| vuln.aliases.unwrap_or_default().into_iter().find(|alias| alias.starts_with("CVE-")));
        let severity = vuln
            .database_specific
            .as_ref()
            .and_then(|value| value.get("severity"))
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_string();
        let fixed_version = vuln.affected.as_ref().and_then(|affected| extract_fixed_version(affected));
        let url = vuln
            .references
            .as_ref()
            .and_then(|refs| refs.first())
            .and_then(|reference| reference.get("url"))
            .and_then(|value| value.as_str())
            .map(str::to_string);
        Self { id, summary: vuln.summary.unwrap_or_default(), severity, fixed_version, url }
    }
}

fn extract_fixed_version(affected: &[serde_json::Value]) -> Option<String> {
    for item in affected {
        let ranges = item.get("ranges")?.as_array()?;
        for range in ranges {
            let events = range.get("events")?.as_array()?;
            for event in events {
                if let Some(fixed) = event.get("fixed").and_then(|value| value.as_str()) {
                    return Some(fixed.to_string());
                }
            }
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct OsvResponse {
    vulns: Option<Vec<OsvVuln>>,
}

#[derive(Debug, Deserialize)]
struct OsvVuln {
    id: Option<String>,
    summary: Option<String>,
    aliases: Option<Vec<String>>,
    database_specific: Option<serde_json::Value>,
    affected: Option<Vec<serde_json::Value>>,
    references: Option<Vec<serde_json::Value>>,
}
