//! 打包链冒烟：事实来源注入后图与分包计划可跑通。
//!
//! 详细语义（导入绑定、重导出、动态边界、隔离模块等）的符合性归属下游产品仓，
//! Boost 只保留最基本 smoke。

use compiler_bundle::{
    graph::build_graph,
    module::ModuleRecord,
    resolve::Target,
    ModuleFactSource, PackDiagnostic, StdSourceReader,
};
use std::path::PathBuf;

struct EmptyFacts;

impl ModuleFactSource for EmptyFacts {
    fn parse_module(&self, label: Option<&str>, _source: &str) -> Result<ModuleRecord, Vec<PackDiagnostic>> {
        Ok(ModuleRecord::empty(label.map(str::to_string)))
    }
}

#[test]
fn graph_builds_from_an_injected_fact_source() {
    let dir = boost_fs::temp::create_folder().expect("temp dir");
    let entry = dir.path().join("entry.js");
    std::fs::write(&entry, "export const answer = 42;\n").expect("write entry");

    let graph = build_graph(&EmptyFacts, &StdSourceReader, &[("main".to_string(), entry.clone())], Target::Browser, dir.path());

    assert_eq!(graph.entries.len(), 1);
    assert_eq!(graph.entries[0].logical, "main");
    assert_eq!(graph.nodes.len(), 1, "an entry with no imports yields one node");
    assert!(graph.diagnostics.is_empty(), "{:?}", graph.diagnostics);
}

#[test]
fn a_missing_entry_is_reported_as_a_diagnostic_instead_of_panicking() {
    let dir = boost_fs::temp::create_folder().expect("temp dir");
    let missing = dir.path().join("missing.js");

    let graph = build_graph(&EmptyFacts, &StdSourceReader, &[("main".to_string(), PathBuf::from(missing))], Target::Browser, dir.path());

    assert!(graph.nodes.is_empty());
    assert_eq!(graph.diagnostics.len(), 1);
}
