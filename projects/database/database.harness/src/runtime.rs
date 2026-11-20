//! 运行期记录机制：高频操作耗时与卡点事件的常开聚合。
//!
//! 这是数据库产品「边跑边调优」的入口。热路径上的埋点（打开、写入、发布、
//! 查询、扫描、`fsync`…）调用 [`Hotspot::record`] 累计墙钟耗时；发现明确卡点
//! （锁等待、磁盘同步停顿、计划退化为全表扫描）时调用 [`Hotspot::stall`] 或
//! [`Hotspot::stall_after`] 显式登记一条带原因的事件。
//!
//! 核心模型位于 `boost_harness::hotspot`（家族共享的运行期记录底座），本模块
//! 给它数据库域的语义与 JSON 报告出口。它与 [`crate::Profiler`] 互补：
//! `Profiler` 负责一次基准运行内的嵌套 span 归因，本模块负责产品进程内持续、
//! 常开的记录——两者都不采样、不读调用栈，结论可复现、可跨机器对比。
//!
//! 机制不影响产品自身运行：关闭时单次埋点只付一次 relaxed 原子读；开启时
//! （默认）付一次时钟差值加一次短锁持有；键数（[`MAX_KEYS`]）与卡点事件数
//! （[`MAX_STALLS`]）都有界，内存不随运行时长增长。宿主（N-API/CLI 产品）
//! 持有进程级 [`Hotspot`] 实例，诊断接口经 [`report_json`] 导出快照，调优
//! 窗口开始前调用 [`Hotspot::reset`] 清空。

pub use boost_harness::hotspot::{Hotspot, HotspotRow, HotspotSnapshot, StallEvent, MAX_KEYS, MAX_STALLS, OVERFLOW_KEY};

/// 把当前快照序列化为 JSON（`camelCase`）。
///
/// 形状是下游诊断接口的稳定合同：顶层含 `enabled`、`uptimeMs`、`rows`、
/// `stalls` 与 `droppedStalls`；`rows` 按 `totalNs` 降序，每行含 `key`、
/// `hits`、`totalNs`、`minNs`、`maxNs`、`avgNs` 与直方图估计的 `p50Ns`、
/// `p95Ns`、`p99Ns`；`stalls` 按 `seq` 升序，每条含 `key`、`cause`、
/// `detail`、`atMs` 与可选 `elapsedNs`。
pub fn report_json(recorder: &Hotspot) -> crate::Result<serde_json::Value> {
    Ok(serde_json::to_value(recorder.snapshot())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn report_json_exposes_rows_and_stalls() {
        let recorder = Hotspot::new();
        recorder.record("db.put.write:AccountRecord", Duration::from_millis(4));
        recorder.record("db.query:AccountRecord", Duration::from_micros(900));
        recorder.stall_after("db.publish", "fsync", "write-through publish", Duration::from_millis(8));
        let report = report_json(&recorder).expect("report");
        assert_eq!(report["enabled"], true);
        let rows = report["rows"].as_array().expect("rows");
        assert_eq!(rows[0]["key"], "db.put.write:AccountRecord", "总耗时最高的键排第一");
        assert_eq!(rows[0]["hits"], 1);
        assert!(rows[0]["totalNs"].as_u64().expect("total ns") >= 4_000_000);
        assert!(rows[0]["p95Ns"].as_u64().expect("p95") >= rows[0]["maxNs"].as_u64().expect("max"));
        let stalls = report["stalls"].as_array().expect("stalls");
        assert_eq!(stalls[0]["cause"], "fsync");
        assert_eq!(stalls[0]["key"], "db.publish");
        assert_eq!(stalls[0]["elapsedNs"], 8_000_000);
    }

    #[test]
    fn a_disabled_recorder_records_nothing_in_json() {
        let recorder = Hotspot::new();
        recorder.set_enabled(false);
        recorder.record("db.put", Duration::from_millis(5));
        let report = report_json(&recorder).expect("report");
        assert_eq!(report["enabled"], false);
        assert!(report["rows"].as_array().expect("rows").is_empty());
    }
}
