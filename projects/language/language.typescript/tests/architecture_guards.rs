//! TypeScript 前端架构负门（架构门 N8）。
//!
//! 断言：TS 公开独立 AST/HIR/MIR 阶段契约；解释器与投影从同一 MIR 分派；
//! 残差 IR 目标无关；OVM adapter 不做语义求解；与 Valkyrie 互不依赖；
//! 未接线的并行表示副本已删除。

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(relative: &str) -> String {
    let path = crate_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("读取 {} 失败: {error}", path.display()))
}

/// 阶段契约：`parse → lower_hir → lower_mir → interpret / specialize → to_odin`
/// 必须全部是 crate 根公开入口。
#[test]
fn public_stage_contract_exists() {
    let lib = read("src/lib.rs");
    for stage in ["pub fn parse(", "pub fn lower_hir(", "pub fn lower_mir(", "pub fn interpret(", "pub fn specialize(", "pub fn to_odin("] {
        assert!(lib.contains(stage), "lib.rs 必须公开阶段契约 `{stage}`");
    }
}

/// 解释器只执行 MIR：不得遍历 AST 节点。
#[test]
fn interpreter_executes_mir_not_ast() {
    let source = read("src/runtime/interpreter.rs");
    for forbidden in ["Statement::", "Expression::"] {
        assert!(!source.contains(forbidden), "interpreter.rs 不得遍历 AST `{forbidden}`——语义入口必须是 MIR");
    }
}

/// OVM adapter 只做残差 → LIR 结构映射：不得内嵌 known-value 求解或语义函数。
#[test]
fn ovm_adapter_does_not_solve_semantics() {
    let source = read("src/ovm/mod.rs");
    for forbidden in ["eval_known", "inline_eval", "semantics::", "fold_binary"] {
        assert!(!source.contains(forbidden), "ovm adapter 不得残留语义求解 `{forbidden}`——残差判定归 specialize");
    }
}

/// 残差 IR 目标无关：不得出现 Odin 头码、emitter 类型或 `ts.*` provider 符号。
#[test]
fn residual_ir_is_target_independent() {
    let source = read("src/residual/mod.rs");
    for forbidden in ["OdinHeadCode", "BytecodeEmitter", "\"ts.", "compiler_emitter", "data_binary_odin"] {
        assert!(!source.contains(forbidden), "residual IR 不得携带目标词汇 `{forbidden}`");
    }
}

/// 特化器消费 MIR，不消费 AST。
#[test]
fn specializer_consumes_mir_not_ast() {
    let source = read("src/specialize/mod.rs");
    for forbidden in ["Statement::", "Expression::"] {
        assert!(!source.contains(forbidden), "specialize.rs 不得遍历 AST `{forbidden}`");
    }
}

/// 双语言互不依赖：TS 不吃 Valkyrie，Valkyrie 不吃 TS。
#[test]
fn languages_are_mutually_independent() {
    let ts_manifest = read("Cargo.toml");
    assert!(!ts_manifest.contains("language-valkyrie"), "TS 不得依赖 language-valkyrie");
    let vk_manifest_path = crate_root().join("../language.valkyrie/Cargo.toml");
    let vk_manifest = fs::read_to_string(&vk_manifest_path).unwrap_or_else(|error| panic!("读取 {} 失败: {error}", vk_manifest_path.display()));
    assert!(!vk_manifest.contains("language-typescript"), "Valkyrie 不得依赖 language-typescript");
}

/// 未接线的并行表示副本已删除（目录名 `ir` 不等于 MIR 已成立，反向亦然）。
#[test]
fn parallel_ir_copies_are_deleted() {
    let ir_root = crate_root().join("../language.typescript.ir/src");
    for dead in ["ast_to_ir.rs", "optimization", "performance", "profile"] {
        let path = ir_root.join(dead);
        assert!(!Path::new(&path).exists(), "并行表示副本 `{dead}` 必须已删除，不得复活");
    }
}
