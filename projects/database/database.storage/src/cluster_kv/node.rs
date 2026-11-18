//! 节点目录里的分片文件：命名与按已发布分片集合打开。

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use database_types::cluster::{Result, SHARD_SUFFIX, ShardId};

use super::FileShard;

/// 一个分片在该节点目录中的 `.yykv` 路径。
pub fn shard_path(directory: &Path, shard: &ShardId) -> PathBuf {
    directory.join(format!("{}.{}", shard.0, SHARD_SUFFIX))
}

/// 按已发布的分片集合打开节点目录里的每个 `.yykv` 分片。
///
/// 路由图由 `database.cluster` 发布，本函数只负责文件：分片 id 必须是单段名称，
/// 缺失的分片文件会被创建成空分片（节点首启情形）。
pub fn open_shards<'a>(directory: &Path, shards: impl IntoIterator<Item = &'a ShardId>) -> Result<HashMap<ShardId, FileShard>> {
    let mut opened = HashMap::new();
    for shard in shards {
        let id = ShardId::try_new(shard.0.clone())?;
        let file = FileShard::open(shard_path(directory, &id))?;
        opened.insert(id, file);
    }
    Ok(opened)
}
