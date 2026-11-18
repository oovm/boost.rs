use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 注册表返回的包元数据。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Package {
    /// 完整包名。
    pub name: String,
    /// 解析得到的语义版本字符串。
    pub version: String,
    /// 包的简短描述。
    #[serde(default)]
    pub description: String,
    /// 项目主页地址。
    #[serde(default)]
    pub homepage: String,
    /// 主要作者或维护者标签。
    #[serde(default)]
    pub author: String,
    /// SPDX 或渠道自有的许可证标识。
    #[serde(default)]
    pub license: String,
    /// 直接依赖名（扁平的遗留形式）。
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// 依赖名到版本约束的映射。
    #[serde(default)]
    pub dependency_versions: BTreeMap<String, String>,
    /// 目标三元组或平台到兼容版本的映射。
    #[serde(default)]
    pub target_conditions: BTreeMap<String, Vec<String>>,
    /// 渠道给出制品地址时的下载 URL。
    pub dist_tarball: Option<String>,
    /// 与 `dist_tarball` 对应的子资源完整性摘要。
    pub dist_integrity: Option<String>,
    /// 是否以 SDK 模块形式发布（JSR 风格）。
    #[serde(default)]
    pub is_sdk_package: bool,
    /// `is_sdk_package` 为真时的 SDK 模块导出名。
    pub sdk_module_name: Option<String>,
    /// 可选的 peer 依赖约束。
    pub peer_dependencies: Option<BTreeMap<String, String>>,
}

/// 发布时的版本递增方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionBump {
    /// 递增 patch 段。
    Patch,
    /// 递增 minor 段。
    Minor,
    /// 递增 major 段。
    Major,
}

impl VersionBump {
    /// 解析 `patch` / `minor` / `major` 关键字。
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "patch" => Some(Self::Patch),
            "minor" => Some(Self::Minor),
            "major" => Some(Self::Major),
            _ => None,
        }
    }
}

/// 控制一次包发布的选项。
///
/// 认证材料由宿主显式写入 [`Self::auth_token`]；本 crate 不读取任何用户凭据存储。
#[derive(Debug, Clone)]
pub struct PublishOptions {
    /// 待发布的包名。
    pub package_name: String,
    /// 待发布的版本号。
    pub version: String,
    /// 发布时的描述覆盖值。
    pub description: String,
    /// 可选的主页覆盖值。
    pub homepage: Option<String>,
    /// 可选的许可证覆盖值。
    pub license: Option<String>,
    /// 本地打包产物目录或 tarball 根目录。
    pub package_path: String,
    /// 注册表适配器逻辑名（例如 `npm`）。
    pub registry_name: String,
    /// Bearer 令牌或发布密钥材料。
    pub auth_token: Option<String>,
    /// 渠道支持时的分发标签（npm 风格）。
    pub tag: Option<String>,
    /// 渠道支持时的访问级别（`public` / `restricted`）。
    pub access: Option<String>,
    /// 上传前自动应用的语义版本递增。
    pub bump: Option<VersionBump>,
    /// 发布成功后是否创建 git 标签。
    pub create_git_tag: bool,
    /// 生成的 git 标签前缀。
    pub git_tag_prefix: Option<String>,
    /// 发布前是否跳过工作树干净检查。
    pub skip_git_check: bool,
    /// 存在 `prepublish` 脚本时是否执行。
    pub run_pre_publish_script: bool,
    /// 只校验与打包，不上传。
    pub dry_run: bool,
    /// 设置后从该目录打包，而不是从项目根目录。
    pub artifact_dir: Option<String>,
    /// 合并进发布 tarball 的 `files` glob 模式。
    pub include_files: Vec<String>,
    /// JSR 的 tarball 使用扁平根布局（不带 `package/` 前缀）。
    pub flat_layout: bool,
}

impl Default for PublishOptions {
    fn default() -> Self {
        Self {
            package_name: String::new(),
            version: String::new(),
            description: String::new(),
            homepage: None,
            license: None,
            package_path: String::new(),
            registry_name: String::new(),
            auth_token: None,
            tag: Some("latest".to_string()),
            access: Some("public".to_string()),
            bump: None,
            create_git_tag: true,
            git_tag_prefix: Some("v".to_string()),
            skip_git_check: false,
            run_pre_publish_script: true,
            dry_run: false,
            artifact_dir: None,
            include_files: Vec::new(),
            flat_layout: false,
        }
    }
}

/// 一次发布尝试的结果。
#[derive(Debug, Clone, Default)]
pub struct PublishResult {
    /// 注册表是否接受上传（或 dry-run 是否成功）。
    pub success: bool,
    /// 已发布的包名。
    pub package_name: String,
    /// 已发布的版本号。
    pub version: String,
    /// 可读的状态或错误摘要。
    pub message: String,
    /// 新版本的规范注册表地址。
    pub published_url: Option<String>,
    /// 是否为 dry run。
    pub dry_run: bool,
    /// 已上传制品的 SHA-256 摘要。
    pub sha256: Option<String>,
    /// 已上传制品的字节数。
    pub size: Option<usize>,
    /// 上传包中文件的个数。
    pub file_count: Option<usize>,
    /// 渠道要求改用官方工具发布时为真。
    pub official_tool_required: bool,
}

impl PublishResult {
    /// 构造「必须使用官方工具」的结构化结果。
    pub fn official_tool(
        package_name: impl Into<String>,
        version: impl Into<String>,
        message: impl Into<String>,
        published_url: Option<String>,
        size: usize,
    ) -> Self {
        Self {
            success: false,
            package_name: package_name.into(),
            version: version.into(),
            message: message.into(),
            published_url,
            dry_run: false,
            sha256: None,
            size: Some(size),
            file_count: None,
            official_tool_required: true,
        }
    }
}

/// 令牌校验结果。
#[derive(Debug, Clone)]
pub struct TokenVerifyResult {
    /// 令牌对该渠道是否有效。
    pub valid: bool,
    /// 有效时的认证用户名或发布者 id。
    pub username: Option<String>,
    /// 无效时的错误详情。
    pub error_message: Option<String>,
}

impl TokenVerifyResult {
    /// 有效令牌及对应身份。
    pub fn success(username: impl Into<String>) -> Self {
        Self { valid: true, username: Some(username.into()), error_message: None }
    }

    /// 无效或被拒绝的令牌。
    pub fn failure(message: impl Into<String>) -> Self {
        Self { valid: false, username: None, error_message: Some(message.into()) }
    }
}

/// 注册表 HTTP 调用的重试配置。
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// 首次请求之后的最大重试次数。
    pub max_retries: u32,
    /// 初始退避延迟（毫秒）。
    pub initial_delay_ms: u64,
    /// 每次失败后延迟的乘数。
    pub backoff_multiplier: f64,
    /// 退避延迟上限（毫秒）。
    pub max_delay_ms: u64,
    /// TCP 连接超时（秒）。
    pub connect_timeout_secs: u64,
    /// 普通请求超时（秒）。
    pub request_timeout_secs: u64,
    /// 上传请求超时（秒）。
    pub upload_timeout_secs: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay_ms: 500,
            backoff_multiplier: 2.0,
            max_delay_ms: 10_000,
            connect_timeout_secs: 30,
            request_timeout_secs: 60,
            upload_timeout_secs: 600,
        }
    }
}

impl RetryConfig {
    /// npm 兼容渠道使用的策略。
    pub fn npm() -> Self {
        Self { max_retries: 5, initial_delay_ms: 500, backoff_multiplier: 2.0, max_delay_ms: 15_000, ..Self::default() }
    }
}
