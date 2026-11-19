//! 集群目录的正式 schema 变更入口:锁定 → 校验候选 → 原子发布 → 内存交换。
//!
//! 协议适配层与产品门面不再自行 catalog lock / change / flush;schema 变更的
//! 锁、校验、发布与失败恢复统一由本模块拥有。发布复用
//! `database_storage::local::publish::publish_atomic`,失败时目录文件保持
//! 发布前内容,内存状态只在文件提交成功后才交换,不存在半发布。

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use database_cluster::catalog::Catalog;
use database_storage::local::publish::publish_atomic;
use database_types::cluster::{CatalogSchema, Result};

/// 目录 schema 的唯一变更入口,可跨连接与线程共享。
///
/// 克隆得到同一份 catalog 状态的另一个句柄。每次变更先持有 catalog 锁,在候选
/// 副本上完成校验与编码,文件型目录先把候选字节原子发布到磁盘,成功后才交换
/// 内存状态;任何一步失败都让文件与内存维持原状。
#[derive(Debug, Clone)]
pub struct CatalogSchemaService {
    catalog: Arc<Mutex<Catalog>>,
}

impl CatalogSchemaService {
    /// 打开(或新建)文件型目录。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self { catalog: Arc::new(Mutex::new(Catalog::open(path)?)) })
    }

    /// 建立纯内存目录,任何操作都不触碰文件系统。
    pub fn memory() -> Self {
        Self { catalog: Arc::new(Mutex::new(Catalog::open_memory())) }
    }

    /// 包装既有目录,保留其 backing path(若有)。
    pub fn from_catalog(catalog: Catalog) -> Self {
        Self { catalog: Arc::new(Mutex::new(catalog)) }
    }

    /// 保存或核对 schema 真值,文件型目录先原子发布再交换内存。
    ///
    /// 与 [`Catalog::ensure_schema`] 同样的幂等与冲突语义:空目录接受新 schema,
    /// 已发布 schema 只接受完全一致的重复请求,版本或文档不一致返回
    /// `Error::SchemaConflict` 且文件与内存都不动。
    pub fn ensure_schema(&self, version: u32, document: &str) -> Result<()> {
        let mut guard = self.catalog.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut candidate = guard.clone();
        candidate.ensure_schema(version, document)?;
        let bytes = candidate.encode_bytes()?;
        if let Some(path) = candidate.path() {
            publish_atomic(path, &bytes)?;
        }
        *guard = candidate;
        Ok(())
    }

    /// 当前已发布的目录 schema(若存在)。
    pub fn schema(&self) -> Option<CatalogSchema> {
        let guard = self.catalog.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.schema().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use database_types::cluster::Error;
    use std::fs;

    const VOS: &str = "table User {\n    @@id: uuid,\n}\n";
    const VOS_OTHER: &str = "table Post {\n    @@id: uuid,\n}\n";

    fn temp_catalog_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("execution-schema-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).expect("tempdir");
        database_cluster::catalog::catalog_path(dir.join("cluster"))
    }

    #[test]
    fn repeated_ensure_is_idempotent_and_byte_stable() {
        let path = temp_catalog_path("idempotent");
        let _ = fs::remove_file(&path);
        let service = CatalogSchemaService::open(&path).expect("open");
        service.ensure_schema(1, VOS).expect("first publish");
        let first = fs::read(&path).expect("read after first");
        service.ensure_schema(1, VOS).expect("replay must be a no-op");
        let second = fs::read(&path).expect("read after replay");
        assert_eq!(first, second, "replaying the same schema must not change the published bytes");
        assert_eq!(service.schema().expect("schema").version, 1);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn conflict_leaves_file_and_memory_untouched() {
        let path = temp_catalog_path("conflict");
        let _ = fs::remove_file(&path);
        let service = CatalogSchemaService::open(&path).expect("open");
        service.ensure_schema(1, VOS).expect("first publish");
        let published = fs::read(&path).expect("read after first");

        let error = service.ensure_schema(2, VOS).expect_err("version bump must conflict");
        assert!(matches!(error, Error::SchemaConflict { expected: 2, found: 1 }), "unexpected error: {error}");
        let error = service.ensure_schema(1, VOS_OTHER).expect_err("different document must conflict");
        assert!(matches!(error, Error::SchemaConflict { .. }), "unexpected error: {error}");

        assert_eq!(fs::read(&path).expect("read after conflicts"), published, "failed changes must not rewrite the file");
        let schema = service.schema().expect("schema");
        assert_eq!((schema.version, schema.document.as_str()), (1, VOS), "failed changes must not swap memory state");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn published_schema_reopens_from_disk() {
        let path = temp_catalog_path("reopen");
        let _ = fs::remove_file(&path);
        CatalogSchemaService::open(&path).expect("open").ensure_schema(3, VOS).expect("publish");

        let reopened = CatalogSchemaService::open(&path).expect("reopen");
        let schema = reopened.schema().expect("schema after reopen");
        assert_eq!((schema.version, schema.document.as_str()), (3, VOS));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn memory_service_never_touches_the_filesystem() {
        let service = CatalogSchemaService::memory();
        service.ensure_schema(1, VOS).expect("in-memory publish");
        assert_eq!(service.schema().expect("schema").version, 1);
        assert!(service.ensure_schema(2, VOS).is_err(), "conflict semantics hold in memory mode too");
    }
}
