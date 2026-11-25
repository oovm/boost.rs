//! Static guards for the Canonical Semantic MIR boundary.

use std::{fs, path::PathBuf};

fn read(relative: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = root.join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn production_source(source: String) -> String {
    source.split("#[cfg(test)]").next().expect("source prefix must exist").to_string()
}

#[test]
fn formal_semantic_path_forbids_semantic_reconstruction_patterns() {
    let sources = vec![(
        "SMIR-G001",
        "name-based semantic dispatch".to_string(),
        production_source(read("src/lowering/features/semantic_mir_contract.rs")),
    )];
    let forbidden = [
        ("SMIR-G001", ".symbol.contains("),
        ("SMIR-G001", ".symbol.starts_with("),
        ("SMIR-G001", ".symbol.ends_with("),
        ("SMIR-G001", "\"von\""),
        ("SMIR-G001", "\"Option\""),
        ("SMIR-G001", "\"Result\""),
        ("SMIR-G002", "descriptor"),
        ("SMIR-G003", "infer_output"),
        ("SMIR-G004", "operand_stack"),
        ("SMIR-G004", "stack_shape"),
        ("SMIR-G005", "java.lang.String"),
        ("SMIR-G005", "System.String"),
        ("SMIR-G005", "JS string"),
        ("SMIR-G005", "WASM_GC_ANYREF"),
        ("SMIR-G006", "unwrap_or(SemanticType::Integer32"),
        ("SMIR-G006", "unwrap_or(ValkyrieType::Integer32"),
        ("SMIR-G006", "default_i32"),
        ("SMIR-G006", "default_void"),
        ("SMIR-G006", "default_object"),
        ("SMIR-G006", "default_anyref"),
    ];
    for (rule, description, source) in sources {
        for (forbidden_rule, pattern) in forbidden {
            if rule == forbidden_rule || matches!(forbidden_rule, "SMIR-G003" | "SMIR-G004" | "SMIR-G006") {
                assert!(
                    !source.contains(pattern),
                    "{rule}: {description} must not contain forbidden semantic reconstruction pattern `{pattern}`"
                );
            }
        }
    }
}

#[test]
fn wasm_backend_entries_gate_before_preparation() {
    let path = "src/lowering/backends/wasm/mod.rs";
    let source = read(path);
    let gate = "validate_submission(submission)";
    let first_preparation = "validate_text_encoding_projection(submission, host_boundary)";
    let gate_at = source.find(gate).unwrap_or_else(|| panic!("{path} has no shared Semantic MIR gate"));
    let preparation_at = source.find(first_preparation).unwrap_or_else(|| panic!("{path} has no preparation marker"));
    assert!(gate_at < preparation_at, "{path} must validate Semantic MIR before backend preparation");
}

#[test]
fn machine_backend_entries_gate_before_lowering() {
    // 三条机器码交付（进程镜像 / C ABI 库 / 宿主扩展）都必须先过同一道
    // Semantic MIR 合同门；不再因为「有自己的入口/导出/布局检查」而绕开。
    let entries = [
        ("src/lowering/backends/machine/mod.rs", "match target.arch {"),
        ("src/lowering/backends/machine/library.rs", "collect_reachable_functions(submission)"),
        ("src/lowering/backends/machine/extension.rs", "let (platform, host) = match target {"),
    ];
    for (path, lowering_marker) in entries {
        let source = read(path);
        let gate_at = source
            .find("validate_machine_submission(submission)?")
            .unwrap_or_else(|| panic!("{path} has no machine submission gate"));
        let lowering_at = source.find(lowering_marker).unwrap_or_else(|| panic!("{path} has no lowering marker `{lowering_marker}`"));
        assert!(gate_at < lowering_at, "{path} must validate Semantic MIR before machine lowering");
    }
}
