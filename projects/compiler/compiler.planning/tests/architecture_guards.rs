//! 规划层架构负门（架构门 N8）。
//!
//! 分区规划只消费语言前端提交的窄事实；禁止重新引入伪公共程序
//! （`structured_terms` 为空的 `ObjectAlgebraicProgram`）与 optimizer 往返。

use std::fs;
use std::path::PathBuf;

fn src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn collect_rs_files(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("目录必须可读") {
        let path = entry.expect("目录项").path();
        if path.is_dir() {
            collect_rs_files(&path, files);
        }
        else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

/// 规划生产代码不得构造公共理论对象或走 optimizer 会话往返。
///
/// 注释与 fail-closed 错误文案允许点名历史形状；构造点与 import 不允许。
#[test]
fn planning_must_not_synthesize_shared_program_or_optimizer_roundtrip() {
    let mut files = Vec::new();
    collect_rs_files(&src_dir(), &mut files);
    assert!(!files.is_empty(), "compiler-planning src must exist");
    for file in files {
        let source = fs::read_to_string(&file).unwrap_or_else(|error| panic!("failed to read {}: {error}", file.display()));
        for forbidden in ["ObjectAlgebraicProgram {", "OptimizationSession", "OptimizationRequest {", "OptimizationResult"] {
            assert!(!source.contains(forbidden), "{} must not construct the shared-program shell (`{forbidden}`)", file.display());
        }
    }
}

/// 规划输入面不得再借用整张 `CanonicalFragment` 表或旧 God IR 形状名。
///
/// 文档注释允许点名历史形状说明「不再接收」；代码行不允许。
#[test]
fn planning_input_must_stay_narrow() {
    let mut files = Vec::new();
    collect_rs_files(&src_dir(), &mut files);
    for file in files {
        let source = fs::read_to_string(&file).unwrap_or_else(|error| panic!("failed to read {}: {error}", file.display()));
        for line in source.lines() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for forbidden in ["CanonicalFragment", "LinkedSemanticProgram", "TargetExecutionPlan", "FragmentSubmission"] {
                assert!(!line.contains(forbidden), "{} 代码行不得借用 `{forbidden}` 进规划输入: {}", file.display(), line.trim());
            }
        }
    }
}
