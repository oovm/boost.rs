//! `panduck.report/v0` 的结构断言。

use std::fs;

use document_convert::{ConvertOutput, ConvertProjectOutput, PublishedMarkdownProject, SCHEMA_REPORT};
use serde_json::Value;

/// 解析报告 JSON。
pub fn parse_report_json(report_json: &str) -> Value {
    serde_json::from_str(report_json).expect("parse panduck.report.json")
}

/// 断言单文件 `convert` 报告与 IR 损失、写出器元数据一致。
pub fn assert_convert_report(output: &ConvertOutput, writer: &str) {
    let report = parse_report_json(&output.report_json);
    assert_eq!(report["schema_version"], SCHEMA_REPORT);
    assert_eq!(report["operation"], "convert");

    let losses = report["losses"].as_array().expect("losses array");
    let coverage_losses = report["coverage"]["loss"].as_array().expect("coverage.loss array");
    assert_eq!(output.loss_count, losses.len());
    assert_losses_mirror_coverage(losses, coverage_losses);

    // status 是能力就绪与 IR 覆盖完整性的与；当前所有路由在导入/导出侧都报 Partial，
    // 因此实际会出现 partial_capability 后缀，关键不变量是它不是失败状态。
    let status = report["status"].as_str().expect("status string");
    assert!(matches!(status, "success" | "success_with_loss"), "unexpected report status {status}");

    assert!(report["coverage"].is_object());
    assert!(report["diagnostics"].is_object());

    assert_eq!(report["pipeline"]["writer"], writer);

    let outputs = report["outputs"].as_array().expect("outputs array");
    assert!(
        outputs.iter().any(|entry| entry["format"] == writer && entry["published"] == true),
        "expected published output for format {writer}, got {outputs:?}"
    );
}

/// 断言 `panduck.report/v0` 与 IR 损失、落盘报告和工程输出字段一致。
pub fn assert_convert_project_report(output: &ConvertProjectOutput, published: &PublishedMarkdownProject) {
    let report = parse_report_json(&output.report_json);
    let disk = fs::read_to_string(&published.report_path).expect("read panduck.report.json");
    let disk_report = parse_report_json(&disk);
    assert_eq!(report, disk_report, "in-memory and on-disk report must match");

    assert_eq!(report["schema_version"], SCHEMA_REPORT);
    assert_eq!(report["operation"], "convert_project");

    let losses = report["losses"].as_array().expect("losses array");
    let coverage_losses = report["coverage"]["loss"].as_array().expect("coverage.loss array");
    assert_eq!(output.loss_count, losses.len());
    assert_losses_mirror_coverage(losses, coverage_losses);

    let unresolved = report["unresolved_assets"].as_array().expect("unresolved_assets array");
    assert_eq!(output.unresolved_assets.len(), unresolved.len(), "report unresolved_assets must mirror project export");
    for source in &output.unresolved_assets {
        assert!(unresolved.iter().any(|value| value.as_str() == Some(source)), "missing unresolved asset {source} in report");
    }

    let status = report["status"].as_str().expect("status string");
    assert!(matches!(status, "success" | "success_with_loss"), "unexpected report status {status}");

    let outputs = report["outputs"].as_array().expect("outputs array");
    assert!(outputs.iter().any(|entry| entry["path"] == "index.md"));
    assert!(outputs.iter().any(|entry| entry["path"] == "panduck.report.json"));

    let published_asset_count = outputs
        .iter()
        .find(|entry| entry["path"] == "assets/")
        .and_then(|entry| entry["published_asset_count"].as_u64())
        .expect("assets output summary");
    assert_eq!(published_asset_count as usize, output.published_assets.len());
    assert_eq!(published.asset_paths.len(), output.published_assets.len());
    assert_eq!(published.chapter_paths.len(), output.published_chapters.len());
}

/// 断言报告至少列出每个预期损失码。
pub fn assert_report_contains_loss_codes(report_json: &str, codes: &[&str]) {
    let report = parse_report_json(report_json);
    let loss_codes = report["losses"].as_array().expect("losses array").iter().filter_map(|loss| loss["code"].as_str()).collect::<Vec<_>>();
    for code in codes {
        assert!(loss_codes.iter().any(|entry| *entry == *code), "expected loss code {code}, got {loss_codes:?}");
    }
}

/// 报告损失逐条镜像 coverage 标记并附带授权 span。
pub fn assert_losses_mirror_coverage(losses: &[Value], coverage_losses: &[Value]) {
    assert_eq!(losses.len(), coverage_losses.len(), "report losses must mirror graph.coverage.loss");
    for (loss, coverage) in losses.iter().zip(coverage_losses) {
        for key in ["code", "message", "status"] {
            assert_eq!(loss[key], coverage[key], "report loss `{key}` must mirror graph.coverage.loss");
        }
        let span = loss.get("span").expect("report losses carry the span key");
        if !span.is_null() {
            assert!(span["start"].as_u64().is_some(), "span.start must be a number: {loss}");
            assert!(span["end"].as_u64().is_some(), "span.end must be a number: {loss}");
            assert!(span["start"].as_u64() < span["end"].as_u64(), "span must be a non-empty range: {loss}");
        }
    }
}
