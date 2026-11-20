//! 运行期记录机制：云调用耗时与卡点事件的常开聚合。
//!
//! 与 `database-harness` 的 `runtime` 共用同一个核心模型
//! （`boost_harness::hotspot`），这里给出云域语义：埋点记录每次调用的墙钟
//! 耗时（签名、传输、解析可各自成键），卡点登记明确原因（`timeout`、
//! `rate-limit`、`dns`、`tls`）。
//!
//! 机制不影响产品自身运行：关闭时单次埋点只付一次 relaxed 原子读；开启时
//! （默认）付一次时钟差值加一次短锁持有；键数与事件数都有界，内存不随运行
//! 时长增长。宿主持有进程级 [`Hotspot`]，诊断接口经 [`report_json`] 导出，
//! 调优窗口开始前调用 [`Hotspot::reset`] 清空。

pub use boost_harness::hotspot::{Hotspot, HotspotRow, HotspotSnapshot, StallEvent, MAX_KEYS, MAX_STALLS, OVERFLOW_KEY};

/// 把当前快照序列化为 JSON（`camelCase`，与 `database-harness` 同形状）。
pub fn report_json(recorder: &Hotspot) -> serde_json::Result<serde_json::Value> {
    serde_json::to_value(recorder.snapshot())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn report_json_exposes_cloud_rows_and_stalls() {
        let recorder = Hotspot::new();
        recorder.record("cloud.sms.send", Duration::from_millis(120));
        recorder.record("cloud.cos.put", Duration::from_millis(40));
        recorder.stall_after("cloud.sms.send", "timeout", "read timeout after 30s", Duration::from_secs(30));
        let report = report_json(&recorder).expect("report");
        let rows = report["rows"].as_array().expect("rows");
        assert_eq!(rows[0]["key"], "cloud.sms.send", "总耗时最高的键排第一");
        assert!(rows[0]["totalNs"].as_u64().expect("total ns") >= 120_000_000);
        let stalls = report["stalls"].as_array().expect("stalls");
        assert_eq!(stalls[0]["cause"], "timeout");
        assert_eq!(stalls[0]["elapsedNs"], 30_000_000_000u64);
    }

    #[test]
    fn a_disabled_recorder_records_nothing() {
        let recorder = Hotspot::new();
        recorder.set_enabled(false);
        recorder.record("cloud.sms.send", Duration::from_millis(5));
        let report = report_json(&recorder).expect("report");
        assert_eq!(report["enabled"], false);
        assert!(report["rows"].as_array().expect("rows").is_empty());
    }
}
