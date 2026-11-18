//! 断点状态：下一次运行从上次停下的位置继续。
//!
//! 状态只保存「每张表走到哪里」，不保存数据本身。写入是「临时文件 + 替换」，
//! 且所有写入都可重放（upsert），因此进程在任意批次之间被杀掉都不会损坏数据。

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::{Error, Row};

const FORMAT: u32 = 1;

/// 一张表当前所处的阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// 正在做首轮全量扫描。
    Full,
    /// 全量已完成，之后只按 `updated_at` 水位重放增量。
    Incremental,
}

/// 一张表的断点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableState {
    /// 当前阶段。
    pub phase: Phase,
    /// 全量开始时的源端时间，作为增量下界，保证全量期间的写入会被重放。
    pub watermark: String,
    /// keyset 游标：上一批最后一行的排序列取值，`None` 表示从头开始。
    pub cursor: Option<Row>,
    /// 本次生命周期内累计写入的目标行数。
    pub written: u64,
}

/// 一份完整的状态文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationState {
    /// 状态格式版本，未知版本直接拒绝。
    pub format: u32,
    /// 按表名索引的断点。
    pub tables: BTreeMap<String, TableState>,
}

impl Default for MigrationState {
    fn default() -> Self {
        Self { format: FORMAT, tables: BTreeMap::new() }
    }
}

impl MigrationState {
    /// 读取状态；文件不存在时返回空状态。
    pub fn load(path: &Path) -> Result<Self, Error> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path).map_err(|error| Error::State(format!("read {}: {error}", path.display())))?;
        let state: Self = serde_json::from_str(&text)
            .map_err(|error| Error::State(format!("parse {}: {error}", path.display())))?;
        if state.format != FORMAT {
            return Err(Error::State(format!(
                "{} uses state format {} but this build reads {FORMAT}",
                path.display(),
                state.format
            )));
        }
        Ok(state)
    }

    /// 原子替换状态文件。
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let text = serde_json::to_string_pretty(self).map_err(|error| Error::State(format!("encode state: {error}")))?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, text).map_err(|error| Error::State(format!("write {}: {error}", temporary.display())))?;
        // Windows 的 `rename` 不覆盖已存在的目标，所以先删旧文件。中间的窗口期
        // 不影响正确性：丢状态只会让下一次从更早的游标重放，写入本身是 upsert。
        if path.exists() {
            fs::remove_file(path).map_err(|error| Error::State(format!("replace {}: {error}", path.display())))?;
        }
        fs::rename(&temporary, path).map_err(|error| Error::State(format!("replace {}: {error}", path.display())))?;
        Ok(())
    }

    /// 取一张表的断点，缺省时按「尚未开始全量」返回。
    pub fn entry(&self, table: &str) -> Option<&TableState> {
        self.tables.get(table)
    }
}
