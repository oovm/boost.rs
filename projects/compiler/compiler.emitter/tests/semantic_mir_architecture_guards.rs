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
fn x64_assembly_service_signature_is_target_private() {
    // 纵链第二段（编码 + 容器装配）只接受目标私有输入：签名里不得出现跨目标
    // 共享程序的任何形状。唯一程序装配器与容器写出同样不得读取它们。
    let x64 = read("src/lowering/backends/machine/x86_64.rs");
    let start = x64.find("pub(crate) fn assemble_x86_64_program_image").expect("缺少 x64 装配服务入口");
    let signature_end = x64[start..].find('{').map(|offset| start + offset).expect("装配服务缺少函数体");
    let signature = &x64[start..signature_end];
    for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind", "Terminator"] {
        assert!(!signature.contains(banned), "x64 装配服务签名不得出现 `{banned}`");
    }
    for path in ["src/lowering/backends/machine/x64_program.rs", "src/lowering/backends/machine/container.rs"] {
        let source = read(path);
        for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind", "Terminator"] {
            assert!(!source.contains(banned), "{path} 作为目标私有装配层不得引用 `{banned}`");
        }
    }
}

#[test]
fn machine_backend_entries_gate_before_lowering() {
    // 三条机器码交付入口（进程镜像 / C ABI 库 / 宿主扩展）已迁到语言侧直接
    // 消费 MIR：每条入口都必须先闭合目标与交付合同门，再进入逐函数 lowering；
    // 入口文件不得再引用任何跨目标共享程序的形状。
    let machine = "../../language/language.valkyrie/src/compiler/emitter/pipeline/machine";
    let entries = [
        (format!("{machine}/exe.rs"), "match target.arch {"),
        (format!("{machine}/library.rs"), "X64ProductTarget::from_resolved"),
        (format!("{machine}/extension.rs"), "let (platform, host) = match &product"),
    ];
    for (path, gate_marker) in entries {
        let source = read(&path);
        let gate_at = source
            .find(gate_marker)
            .unwrap_or_else(|| panic!("{path} has no machine target gate `{gate_marker}`"));
        let lowering_at = source
            .find("lowerer::lower_function")
            .unwrap_or_else(|| panic!("{path} has no lowering marker `lowerer::lower_function`"));
        assert!(gate_at < lowering_at, "{path} must gate the target contract before machine lowering");
        for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind"] {
            assert!(!source.contains(banned), "{path} 作为语言侧 MIR 直消费入口不得引用 `{banned}`");
        }
    }
}

/// 取出指定函数的完整定义（含签名与函数体），按花括号配对截断。
fn function_definition<'a>(source: &'a str, name: &str) -> &'a str {
    let marker = format!("fn {name}");
    let start = source.find(&marker).unwrap_or_else(|| panic!("source has no function `{name}`"));
    let open = source[start..].find('{').map(|offset| start + offset).expect("function has no body");
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[start..=open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("function `{name}` body is not balanced");
}

#[test]
fn jvm_assembly_service_signature_is_target_private() {
    // JVM 纵链第一段把计划事实冻结成目标私有输入；第二段（类装配 + companion
    // 唯一性自检）只接受该输入，签名与函数体都不得出现跨目标共享程序的形状。
    let emit = read("src/lowering/backends/jvm/emit.rs");
    let assembly = function_definition(&emit, "assemble_jvm_class");
    for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind", "Terminator"] {
        assert!(!assembly.contains(banned), "JVM 装配服务不得引用 `{banned}`");
    }
    // 类编码与 JAR 容器写出同样只吃目标私有输入。
    let backend = read("src/backend/jvm/mod.rs");
    for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind", "Terminator"] {
        assert!(!backend.contains(banned), "src/backend/jvm/mod.rs 作为 JVM 目标私有装配层不得引用 `{banned}`");
    }
}

#[test]
fn clr_assembly_service_signature_is_target_private() {
    // CLR 纵链与 JVM 同构：第一段把计划事实冻结成目标私有输入，第二段只做模块
    // 自检并封装，签名与函数体都不得出现跨目标共享程序的形状。
    let emit = read("src/lowering/backends/clr/emit.rs");
    let assembly = function_definition(&emit, "assemble_clr_module");
    for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind", "Terminator"] {
        assert!(!assembly.contains(banned), "CLR 装配服务不得引用 `{banned}`");
    }
    // `MSIL` 与 PE 容器写出同样只吃目标私有输入。
    for path in ["src/backend/clr/mod.rs", "src/backend/clr/msil_text_writer.rs", "src/backend/clr/host_process.rs"] {
        let backend = read(path);
        for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind", "Terminator"] {
            assert!(!backend.contains(banned), "{path} 作为 CLR 目标私有装配层不得引用 `{banned}`");
        }
    }
}

#[test]
fn wasm_assembly_service_signature_is_target_private() {
    // Wasm 纵链同样两段式：第一段完成合同门与段编码并冻结成目标私有输入，
    // 第二段只做模块形状自检与封装。
    let wasm = read("src/lowering/backends/wasm/mod.rs");
    let assembly = function_definition(&wasm, "assemble_wasm_module");
    for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind", "Terminator"] {
        assert!(!assembly.contains(banned), "WASM 装配服务不得引用 `{banned}`");
    }
    // WASI 打包与 JS glue 宿主壳只消费已编码模块。
    for path in ["src/backend/wasi/mod.rs", "src/backend/wasm_js_glue/mod.rs"] {
        let backend = read(path);
        for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind", "Terminator"] {
            assert!(!backend.contains(banned), "{path} 作为 WASM 目标私有装配层不得引用 `{banned}`");
        }
    }
}

#[test]
fn ovm_assembly_service_signature_is_target_private() {
    // OVM 纵链同样两段式：第一段完成实例门与函数体编码并冻结成目标私有输入，
    // 第二段只做模块形状自检与封装。
    let ovm = read("src/lowering/backends/ovm/mod.rs");
    let assembly = function_definition(&ovm, "assemble_ovm_module");
    for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind", "Terminator"] {
        assert!(!assembly.contains(banned), "OVM 装配服务不得引用 `{banned}`");
    }
    // `.odin` 容器写出只消费已编码模块。
    let backend = read("src/backend/vm/mod.rs");
    for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind", "Terminator"] {
        assert!(!backend.contains(banned), "src/backend/vm/mod.rs 作为 OVM 目标私有装配层不得引用 `{banned}`");
    }
}

#[test]
fn ovm_lane_consumes_mir_without_shared_program() {
    // OVM 纵链第一段已迁到语言侧直读 MIR：入口与 lowering 文件不得再引用
    // 跨目标共享程序的任何形状（MIR 自身的终结符类型不在禁用之列）。
    let ovm = "../../language/language.valkyrie/src/compiler/emitter/pipeline/ovm";
    for path in [format!("{ovm}/mod.rs"), format!("{ovm}/lowerer.rs")] {
        let source = read(&path);
        for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind"] {
            assert!(!source.contains(banned), "{path} 作为语言侧 MIR 直消费入口不得引用 `{banned}`");
        }
    }
    // emitter 侧只保留目标私有编码原语：整个 backends/ovm/ 不得出现公共 IR 类型。
    for path in ["src/lowering/backends/ovm/mod.rs", "src/lowering/backends/ovm/lir.rs", "src/lowering/backends/ovm/operator.rs"] {
        let source = read(path);
        for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind", "Terminator"] {
            assert!(!source.contains(banned), "{path} 作为 OVM 目标私有编码层不得引用 `{banned}`");
        }
    }
}

#[test]
fn jvm_lane_consumes_mir_without_shared_program() {
    // JVM 纵链第一段已迁到语言侧直读 MIR：入口与 lowering 文件不得再引用
    // 跨目标共享程序的任何形状（MIR 自身的终结符类型不在禁用之列）。
    let jvm = "../../language/language.valkyrie/src/compiler/emitter/pipeline/jvm";
    for path in [format!("{jvm}/mod.rs"), format!("{jvm}/lowerer.rs")] {
        let source = read(&path);
        for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind"] {
            assert!(!source.contains(banned), "{path} 作为语言侧 MIR 直消费入口不得引用 `{banned}`");
        }
    }
    // emitter 侧只保留目标私有编码原语与装配服务：整个 backends/jvm/ 不得出现公共 IR 类型。
    for path in ["src/lowering/backends/jvm/mod.rs", "src/lowering/backends/jvm/emit.rs", "src/lowering/backends/jvm/recipe.rs"] {
        let source = read(path);
        for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind", "Terminator"] {
            assert!(!source.contains(banned), "{path} 作为 JVM 目标私有编码层不得引用 `{banned}`");
        }
    }
}

#[test]
fn clr_lane_consumes_mir_without_shared_program() {
    // CLR 纵链第一段已迁到语言侧直读 MIR：入口与 lowering 文件不得再引用
    // 跨目标共享程序的任何形状（MIR 自身的终结符类型不在禁用之列）。
    let clr = "../../language/language.valkyrie/src/compiler/emitter/pipeline/clr";
    for path in [format!("{clr}/mod.rs"), format!("{clr}/lowerer.rs")] {
        let source = read(&path);
        for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind"] {
            assert!(!source.contains(banned), "{path} 作为语言侧 MIR 直消费入口不得引用 `{banned}`");
        }
    }
    // emitter 侧只保留目标私有编码原语与装配服务：整个 backends/clr/ 不得出现公共 IR 类型。
    for path in ["src/lowering/backends/clr/mod.rs", "src/lowering/backends/clr/emit.rs", "src/lowering/backends/clr/mir.rs"] {
        let source = read(path);
        for banned in ["FragmentSubmission", "ExecutableFunction", "InstructionKind", "Terminator"] {
            assert!(!source.contains(banned), "{path} 作为 CLR 目标私有编码层不得引用 `{banned}`");
        }
    }
}
