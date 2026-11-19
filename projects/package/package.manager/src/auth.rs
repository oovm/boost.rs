use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use package_registry::{Registry, TokenVerifyResult};
use package_resolve::ProjectLayout;
use serde::{Deserialize, Serialize};

use crate::{
    DiscoveredCredential, ManagerError, Result,
    credentials::{CredentialProvider, NoCredentials},
};

/// 存储的 vendor 认证令牌（路径来自 [`ProjectLayout`]）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VendorAuthStore {
    #[serde(default)]
    tokens: BTreeMap<String, String>,
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    token_env_vars: &'static [&'static str],
}

impl Default for VendorAuthStore {
    fn default() -> Self {
        Self { tokens: BTreeMap::new(), path: PathBuf::new(), token_env_vars: &[] }
    }
}

impl VendorAuthStore {
    /// 从指定路径打开或创建凭据存储。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_token_envs(path, &[])
    }

    /// 使用工程布局中的 auth 路径与环境变量列表打开存储。
    pub fn open_with_layout(layout: ProjectLayout) -> Result<Self> {
        Self::open_with_token_envs(layout.auth_store_path(), layout.token_env_vars)
    }

    /// 打开存储并登记额外 token 环境变量名（在 `{REGISTRY}_TOKEN` 之后查询）。
    pub fn open_with_token_envs(path: impl AsRef<Path>, token_env_vars: &'static [&'static str]) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if path.is_file() {
            let source = std::fs::read_to_string(&path)?;
            let mut store: VendorAuthStore = data_text_von::from_str(&source).unwrap_or_default();
            store.path = path;
            store.token_env_vars = token_env_vars;
            Ok(store)
        }
        else {
            Ok(Self { tokens: BTreeMap::new(), path, token_env_vars })
        }
    }

    /// 给定布局下的 auth 存储文件路径。
    pub fn path_for(layout: ProjectLayout) -> PathBuf {
        layout.auth_store_path()
    }

    /// 读取注册表 token（不含 endpoint / 项目目录上下文）。
    pub fn get_token(&self, registry: &str) -> Option<String> {
        self.tokens.get(registry).cloned().filter(|token| !token.trim().is_empty())
    }

    /// 写入注册表 token 并持久化。
    pub fn set_token(&mut self, registry: impl Into<String>, token: impl Into<String>) -> Result<()> {
        self.tokens.insert(registry.into(), token.into());
        self.save()
    }

    /// 删除注册表 token；存在并已删除时返回 `true`。
    pub fn remove_token(&mut self, registry: &str) -> Result<bool> {
        let removed = self.tokens.remove(registry).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// 列出已存储 token 的注册表名称。
    pub fn list(&self) -> Vec<String> {
        self.tokens.keys().cloned().collect()
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = render_tokens(&self.tokens);
        std::fs::write(&self.path, content)?;
        Ok(())
    }
}

/// `auth.von` 的写入形状：读取端只接受 `key: "value"`。
fn render_tokens(tokens: &BTreeMap<String, String>) -> String {
    let mut out = String::from("{\n");
    if !tokens.is_empty() {
        out.push_str("    tokens: {\n");
        for (name, token) in tokens {
            out.push_str(&format!("        \"{}\": \"{}\",\n", escape(name), escape(token)));
        }
        out.push_str("    },\n");
    }
    out.push_str("}\n");
    out
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 产品 CLI 登录结果。
#[derive(Debug, Clone)]
pub struct LoginResult {
    /// 注册表 token 校验结果。
    pub verify: TokenVerifyResult,
    /// 凭据来源描述。
    pub credential_source: String,
    /// 手动 token 同步到官方 CLI 配置后的目标路径。
    pub synced_to: Option<String>,
    /// 是否已写入 `auth.von`。
    pub stored_in_auth_von: bool,
}

/// Vendor / 注册表认证操作。
#[derive(Clone)]
pub struct VendorManager {
    /// 底层 vendor 凭据存储。
    pub auth: VendorAuthStore,
    /// 宿主注入的凭据策略。
    pub credentials: Arc<dyn CredentialProvider>,
}

impl VendorManager {
    /// 使用工程布局打开 vendor 管理器（无外部凭据策略）。
    pub fn open_with_layout(layout: ProjectLayout) -> Result<Self> {
        Self::open_with_provider(layout, Arc::new(NoCredentials))
    }

    /// 使用工程布局与显式凭据策略打开 vendor 管理器。
    pub fn open_with_provider(layout: ProjectLayout, credentials: Arc<dyn CredentialProvider>) -> Result<Self> {
        Ok(Self { auth: VendorAuthStore::open_with_layout(layout)?, credentials })
    }

    /// 按优先级发现凭据：`auth.von` → 环境变量 → 宿主策略。
    pub fn discover_credential(&self, registry: &str, endpoint: Option<&str>, project_dir: Option<&Path>) -> Option<DiscoveredCredential> {
        if !self.credentials.uses_external_store(registry) {
            if let Some(token) = self.auth.get_token(registry) {
                return Some(DiscoveredCredential { token, source: format!("auth.von") });
            }
        }
        if let Ok(token) = std::env::var(format!("{}_TOKEN", registry.to_ascii_uppercase())) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                return Some(DiscoveredCredential { token, source: format!("env:{}_TOKEN", registry.to_ascii_uppercase()) });
            }
        }
        self.credentials.discover(registry, endpoint, project_dir)
    }

    /// 登录注册表。`token` 为空时先从凭据存储发现。
    pub fn login(
        &mut self,
        registry: &dyn Registry,
        token: Option<&str>,
        project_dir: Option<&Path>,
        sync_official: bool,
    ) -> Result<LoginResult> {
        let manual = token.filter(|value| !value.trim().is_empty());
        let (token, source) = if let Some(token) = manual {
            (token.to_string(), "手动输入".to_string())
        }
        else if let Some(credential) = self.discover_credential(registry.name(), Some(registry.endpoint()), project_dir) {
            (credential.token, credential.source)
        }
        else {
            return Err(ManagerError::message(format!("未提供认证令牌，且无法发现 {} 凭据；请使用 `login --token <token>`", registry.name())));
        };

        let result = registry.verify_token(&token)?;
        if !result.valid {
            return Ok(LoginResult { verify: result, credential_source: source, synced_to: None, stored_in_auth_von: false });
        }

        let mut stored_in_auth_von = false;
        if self.credentials.uses_external_store(registry.name()) {
            let _ = self.auth.remove_token(registry.name());
        }
        else {
            self.auth.set_token(registry.name(), token.clone())?;
            stored_in_auth_von = true;
        }

        let synced_to = if manual.is_some() && sync_official {
            self.credentials.sync_to_official(registry.name(), Some(registry.endpoint()), &token)?.map(|path| path.display().to_string())
        }
        else {
            None
        };

        Ok(LoginResult { verify: result, credential_source: source, synced_to, stored_in_auth_von })
    }

    /// 登出注册表。
    pub fn logout(&mut self, registry_name: &str) -> Result<bool> {
        self.auth.remove_token(registry_name)
    }

    /// 用当前发现的凭据调用注册表校验，并在用户名后附带来源说明。
    pub fn whoami(&self, registry: &dyn Registry) -> Result<TokenVerifyResult> {
        let Some(credential) = self.discover_credential(registry.name(), Some(registry.endpoint()), None)
        else {
            return Ok(TokenVerifyResult::failure(format!(
                "未找到注册表 {} 的凭据（环境变量 / 官方 CLI 配置{}）",
                registry.name(),
                if self.credentials.uses_external_store(registry.name()) { "" } else { " / auth.von" }
            )));
        };
        let mut result = registry.verify_token(&credential.token)?;
        if result.valid
            && let Some(username) = result.username.as_mut()
        {
            *username = format!("{username} ← {}", credential.source);
        }
        Ok(result)
    }

    /// 列出 `auth.von` 中已保存 token 的注册表名称。
    pub fn list(&self) -> Vec<String> {
        self.auth.list()
    }
}
