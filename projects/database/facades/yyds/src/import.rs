//! 把既有 `.yydb` 记录库提升为 YYDS 节点目录。
//!
//! 行记录在两种宿主里是同一套键（`__yydb/v0/row/<表>/<主键>`）与同一套行编码，
//! 所以提升是逐键搬运而不是重写语义：读 `.yydb` 的 B+ 树，按批 `commit_batch`
//! 落到节点的 `.yykv` 分片。行里引用的内嵌对象（`__yydb/v0/object/<hash>/…`）
//! 一并搬过去，读路径才能解码 out-of-line 的 bytes/vector。
//!
//! schema 不搬——YYDS 节点的真值是 catalog，由宿主在打开节点时发布，避免把
//! 两套 schema 真值带进同一个目录。
//!
//! 这是迁移与灾备的公共入口：`tools/migration` 在完成 VOS 目标迁移后调用它，
//! 已有 `.yydb` 副本也可以直接提升，不必重新连线上库。

use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use database::{
    cluster::catalog::{Catalog, catalog_path},
    execution::cluster::{BufferedCommit, KeyValueExecutor},
    query::cluster_kv::{KeyValueAction, KeyValueCommand},
    storage::local_io::FilePager,
    types::cluster::{Namespace, ShardEpoch, ShardId},
};

use crate::{
    runtime::open_executor,
    vos::VOS_NAMESPACE,
};

/// 行记录键前缀。
const ROW_PREFIX: &[u8] = b"__yydb/v0/row/";
/// 单文件 `.yydb` 内嵌对象清单的分块键前缀。
const OBJECT_PREFIX: &[u8] = b"__yydb/v0/object/";
/// 单次 `commit_batch` 的命令数。
const BATCH: usize = 2_000;
/// 单分片节点的路由 epoch。
const NODE_EPOCH: u64 = 1;
/// 单分片节点的分片 id。
const NODE_SHARD: &str = "node-0";

/// 提升结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportReport {
    /// 搬入的行记录数。
    pub rows: usize,
    /// 搬入的内嵌对象分块数（向量与超长 bytes 的 CAS 载荷）。
    pub objects: usize,
    /// 搬入的字节数。
    pub bytes: usize,
    /// 跳过的非记录键（与宿主无关的元数据）。
    pub skipped: usize,
}

/// 提升失败。
#[derive(Debug)]
pub enum ImportError {
    /// 源 `.yydb` 不存在。
    MissingSource(PathBuf),
    /// 文件系统错误。
    Io(std::io::Error),
    /// 源库读取或节点写入失败。
    Database(database::types::Error),
    /// 节点目录装配失败。
    Node(String),
}

impl fmt::Display for ImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSource(path) => write!(formatter, "源库不存在: {}", path.display()),
            Self::Io(error) => error.fmt(formatter),
            Self::Database(error) => error.fmt(formatter),
            Self::Node(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<std::io::Error> for ImportError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<database::types::Error> for ImportError {
    fn from(error: database::types::Error) -> Self {
        Self::Database(error)
    }
}

impl From<database::types::cluster::Error> for ImportError {
    fn from(error: database::types::cluster::Error) -> Self {
        Self::Node(error.to_string())
    }
}

/// 把 `source` 处的 `.yydb` 行记录搬进 `directory` 的 YYDS 节点分片。
///
/// 重复调用是覆盖式幂等：相同行得到相同字节，节点分片上的其它键不受影响。
pub fn import_yydb(source: &Path, directory: &Path) -> Result<ImportReport, ImportError> {
    if !source.is_file() {
        return Err(ImportError::MissingSource(source.to_path_buf()));
    }
    fs::create_dir_all(directory)?;
    let catalog = ensure_catalog(directory)?;

    // 先读完整份快照再打开节点，避免同目录下两个 pager 抢同一个锁文件。
    let mut pager = FilePager::open(source, true)?;
    let pairs = pager.scan_kv()?;
    drop(pager);

    let executor = open_executor(directory, Some(&catalog)).map_err(|error| ImportError::Node(error.to_string()))?;
    let namespace = Namespace(VOS_NAMESPACE.to_owned());
    let mut report = ImportReport { rows: 0, objects: 0, bytes: 0, skipped: 0 };
    let mut batch: Vec<BufferedCommit> = Vec::with_capacity(BATCH);
    for (key, value) in pairs {
        if key.encoded.starts_with(ROW_PREFIX) {
            report.rows += 1;
        }
        else if key.encoded.starts_with(OBJECT_PREFIX) {
            report.objects += 1;
        }
        else {
            report.skipped += 1;
            continue;
        }
        report.bytes += value.len();
        batch.push(BufferedCommit {
            command: KeyValueCommand { namespace: namespace.clone(), key: key.encoded, action: KeyValueAction::Put(value) },
            condition: None,
        });
        if batch.len() >= BATCH {
            commit(executor.as_ref(), std::mem::take(&mut batch))?;
        }
    }
    commit(executor.as_ref(), batch)?;
    Ok(report)
}

fn commit(executor: &dyn KeyValueExecutor, batch: Vec<BufferedCommit>) -> Result<(), ImportError> {
    if batch.is_empty() {
        return Ok(());
    }
    executor.commit_batch(None, batch).map_err(ImportError::from)
}

fn ensure_catalog(directory: &Path) -> Result<PathBuf, ImportError> {
    let path = catalog_path(directory.join("catalog"));
    if path.exists() {
        return Ok(path);
    }
    let mut catalog = Catalog::open(&path).map_err(ImportError::from)?;
    catalog.publish_routing(ShardEpoch(NODE_EPOCH), vec![ShardId(NODE_SHARD.to_owned())]).map_err(ImportError::from)?;
    catalog.flush().map_err(ImportError::from)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use database::{execution::engine::Connection, types::Value};

    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_nanos();
        let path = std::env::temp_dir().join(format!("yyds-import-{label}-{nonce}"));
        fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[test]
    fn imports_rows_and_skips_metadata() {
        let root = temp_dir("rows");
        let source = root.join("source.yydb");
        let connection = Connection::open(&source).expect("open source");
        connection.ensure_schema_files(&["table Item {\n    @@id: uuid,\n    name: utf8,\n}\n"]).expect("schema");
        for index in 0..5 {
            connection
                .execute_vos_write_with_params(
                    "Item.insert({ id: $id, name: $name })",
                    &BTreeMap::from([
                        ("id".to_owned(), Value::Uuid(uuid::Uuid::now_v7())),
                        ("name".to_owned(), Value::Text(format!("item-{index}"))),
                    ]),
                )
                .expect("insert");
        }
        connection.checkpoint().expect("checkpoint");
        connection.close();

        let node = root.join("node");
        let report = import_yydb(&source, &node).expect("import");
        assert_eq!(report.rows, 5);
        assert_eq!(report.objects, 0);
        assert!(report.bytes > 0);
        assert!(node.join(format!("{NODE_SHARD}.yykv")).is_file(), "rows land in the node shard");

        // Re-running is idempotent byte-wise: the same keys are just overwritten.
        assert_eq!(import_yydb(&source, &node).expect("re-import"), report);

        let _ = fs::remove_dir_all(root);
    }
}
