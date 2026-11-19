//! 包注册表渠道合同与通用适配器（`package.registry`）。
//!
//! # 归属
//!
//! 本 crate 拥有第三方包渠道的通用机制：包身份与版本元数据、制品位置、发布请求与令牌校验，
//! 以及 npm / JSR / NuGet / Maven / Conda 这几类真实渠道的请求规则。
//!
//! 不归本 crate 的内容：
//!
//! - 版本约束选择、lock 生成与依赖图求解（`package.resolve`）。
//! - 下载落盘缓存与离线状态（`package.cache`）。
//! - 发布资格、发布计划与提交语义（`package.publish`）。
//! - 第三方渠道凭证的发现与写回。凭证属于宿主策略，必须由调用方显式注入
//!   [`types::PublishOptions::auth_token`]；本 crate 不扫描 `~/.npmrc`、Deno 配置或任何用户秘密。
//! - 自有渠道（产品自己的注册表服务）的适配器与签名格式，由产品侧持有。
//!
//! # 传输
//!
//! HTTP 走 [`boost_net`] 的同步 HTTP/1.1 传输。代理、DNS 与连接池策略不属于协议层，
//! 需要这些策略的宿主可以用 [`HttpClient::with_transport`] 注入自己的 [`Transport`]。

#![warn(missing_docs)]

mod advisory;
mod artifact;
mod conda;
mod error;
mod http;
mod jsr;
mod maven;
mod npm;
mod nuget;
mod registry;
mod types;

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

pub use advisory::{Advisory, OSV_ENDPOINT, OsvClient, osv_ecosystem};
pub use artifact::{extract_tarball, sha256_hex, verify_sri};
pub use conda::CondaRegistry;
pub use error::RegistryError;
pub use http::{HttpClient, HttpResponse, build_url, proxy_env_hint, url_encode};
pub use jsr::JsrRegistry;
pub use maven::MavenRegistry;
pub use npm::NpmRegistry;
pub use nuget::NugetRegistry;
pub use registry::Registry;
pub use types::{Package, PublishOptions, PublishResult, RetryConfig, TokenVerifyResult, VersionBump};

pub use boost_net::http::{Headers, ProxyConfig, Request, Transport};

/// 构建内置的第三方渠道适配器集合。
pub fn default_registries() -> Result<HashMap<String, Arc<dyn Registry>>, RegistryError> {
    registries_with_endpoints(&BTreeMap::new())
}

/// 按 endpoint 覆盖构建内置适配器集合；缺失的渠道使用各适配器的默认地址。
pub fn registries_with_endpoints(endpoints: &BTreeMap<String, String>) -> Result<HashMap<String, Arc<dyn Registry>>, RegistryError> {
    registries_with_proxy_option(endpoints, None)
}

/// 按 endpoint 覆盖构建内置适配器集合，并让所有渠道走同一个显式代理。
///
/// 是否从环境变量取值由调用方决定，可用 [`ProxyConfig::from_env`]。
pub fn registries_with_proxy(
    endpoints: &BTreeMap<String, String>,
    proxy: ProxyConfig,
) -> Result<HashMap<String, Arc<dyn Registry>>, RegistryError> {
    registries_with_proxy_option(endpoints, Some(proxy))
}

fn registries_with_proxy_option(
    endpoints: &BTreeMap<String, String>,
    proxy: Option<ProxyConfig>,
) -> Result<HashMap<String, Arc<dyn Registry>>, RegistryError> {
    let npm = endpoint_or(endpoints, "npm", NpmRegistry::DEFAULT_ENDPOINT);
    let jsr = endpoint_or(endpoints, "jsr", JsrRegistry::DEFAULT_ENDPOINT);
    let nuget = endpoint_or(endpoints, "nuget", NugetRegistry::DEFAULT_ENDPOINT);
    let maven = endpoint_or(endpoints, "maven", MavenRegistry::DEFAULT_ENDPOINT);
    let conda = endpoint_or(endpoints, "conda", CondaRegistry::DEFAULT_ENDPOINT);

    let mut registries: HashMap<String, Arc<dyn Registry>> = HashMap::new();
    let npm_adapter = match &proxy {
        Some(proxy) => NpmRegistry::with_http(npm, HttpClient::with_proxy(RetryConfig::npm(), proxy.clone())),
        None => NpmRegistry::new(npm)?,
    };
    registries.insert("npm".to_string(), Arc::new(npm_adapter));
    let jsr_adapter = match &proxy {
        Some(proxy) => JsrRegistry::with_http(jsr, HttpClient::with_proxy(RetryConfig::npm(), proxy.clone())),
        None => JsrRegistry::new(jsr)?,
    };
    registries.insert("jsr".to_string(), Arc::new(jsr_adapter));
    let nuget_adapter = match &proxy {
        Some(proxy) => NugetRegistry::with_http(nuget, HttpClient::with_proxy(RetryConfig::default(), proxy.clone())),
        None => NugetRegistry::new(nuget)?,
    };
    registries.insert("nuget".to_string(), Arc::new(nuget_adapter));
    let maven_adapter = match &proxy {
        Some(proxy) => MavenRegistry::with_http(maven, HttpClient::with_proxy(RetryConfig::default(), proxy.clone())),
        None => MavenRegistry::new(maven)?,
    };
    registries.insert("maven".to_string(), Arc::new(maven_adapter));
    let conda_adapter = match &proxy {
        Some(proxy) => CondaRegistry::with_http(conda, HttpClient::with_proxy(RetryConfig::default(), proxy.clone())),
        None => CondaRegistry::new(conda)?,
    };
    registries.insert("conda".to_string(), Arc::new(conda_adapter));
    Ok(registries)
}

fn endpoint_or<'a>(endpoints: &'a BTreeMap<String, String>, name: &str, default: &'a str) -> &'a str {
    endpoints.get(name).map(String::as_str).unwrap_or(default)
}

/// 内存注册表：下游验收与离网场景使用的真实 [`Registry`] 实现。
#[derive(Debug, Default)]
pub struct MockRegistry {
    name: String,
    endpoint: String,
    packages: std::sync::Mutex<HashMap<String, Package>>,
    published: std::sync::Mutex<Vec<(String, String, Vec<u8>)>>,
    tokens: std::sync::Mutex<HashMap<String, String>>,
}

impl MockRegistry {
    /// 以 `name` 与 `endpoint` 建立一个空的内存注册表。
    pub fn new(name: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            packages: std::sync::Mutex::new(HashMap::new()),
            published: std::sync::Mutex::new(Vec::new()),
            tokens: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// 预置一条可供后续查询的包元数据。
    pub fn insert_package(&self, package: Package) {
        let key = format!("{}@{}", package.name, package.version);
        self.packages.lock().expect("packages lock").insert(key, package);
    }

    /// 登记一个 [`Registry::verify_token`] 应当接受的令牌。
    pub fn insert_token(&self, token: impl Into<String>, username: impl Into<String>) {
        self.tokens.lock().expect("tokens lock").insert(token.into(), username.into());
    }

    /// 返回 [`Registry::publish_package`] 记录的发布请求。
    pub fn published(&self) -> Vec<(String, String, Vec<u8>)> {
        self.published.lock().expect("published lock").clone()
    }
}

impl Registry for MockRegistry {
    fn name(&self) -> &str {
        &self.name
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn set_endpoint(&mut self, endpoint: &str) {
        self.endpoint = endpoint.trim_end_matches('/').to_string();
    }

    fn get_package(&self, package_name: &str, version: &str) -> Result<Package, RegistryError> {
        let packages = self.packages.lock().expect("packages lock");
        if version == "latest" {
            return packages
                .values()
                .filter(|package| package.name == package_name)
                .max_by(|left, right| left.version.cmp(&right.version))
                .cloned()
                .ok_or_else(|| RegistryError::NotFound(package_name.to_string()));
        }
        packages.get(&format!("{package_name}@{version}")).cloned().ok_or_else(|| RegistryError::NotFound(format!("{package_name}@{version}")))
    }

    fn search_packages(&self, query: &str) -> Result<Vec<Package>, RegistryError> {
        let packages = self.packages.lock().expect("packages lock");
        Ok(packages.values().filter(|package| package.name.contains(query) || package.description.contains(query)).cloned().collect())
    }

    fn publish_package(&self, options: &PublishOptions, tarball_data: &[u8]) -> Result<PublishResult, RegistryError> {
        self.published.lock().expect("published lock").push((options.package_name.clone(), options.version.clone(), tarball_data.to_vec()));
        let package = Package {
            name: options.package_name.clone(),
            version: options.version.clone(),
            description: options.description.clone(),
            homepage: options.homepage.clone().unwrap_or_default(),
            license: options.license.clone().unwrap_or_default(),
            ..Package::default()
        };
        self.insert_package(package);
        Ok(PublishResult {
            success: true,
            package_name: options.package_name.clone(),
            version: options.version.clone(),
            message: "发布成功".to_string(),
            published_url: Some(format!("{}/{}/{}", self.endpoint, options.package_name, options.version)),
            dry_run: false,
            sha256: Some(sha256_hex(tarball_data)),
            size: Some(tarball_data.len()),
            file_count: None,
            official_tool_required: false,
        })
    }

    fn download_package(&self, package: &Package, target_directory: &std::path::Path) -> Result<String, RegistryError> {
        std::fs::create_dir_all(target_directory)?;
        let manifest = format!("name: \"{}\"\nversion: \"{}\"\n", package.name, package.version);
        std::fs::write(target_directory.join("package.von"), manifest)?;
        Ok(target_directory.display().to_string())
    }

    fn get_package_versions(&self, package_name: &str) -> Result<Vec<String>, RegistryError> {
        let packages = self.packages.lock().expect("packages lock");
        Ok(packages.values().filter(|package| package.name == package_name).map(|package| package.version.clone()).collect())
    }

    fn verify_token(&self, token: &str) -> Result<TokenVerifyResult, RegistryError> {
        let tokens = self.tokens.lock().expect("tokens lock");
        match tokens.get(token) {
            Some(username) => Ok(TokenVerifyResult::success(username.clone())),
            None => Ok(TokenVerifyResult::failure("invalid token")),
        }
    }
}
