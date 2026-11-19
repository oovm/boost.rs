//! 防回流门禁（33 号合同）。
//!
//! 全 workspace 扫描 `src/`，禁止在 owner 之外出现 Kahn 拓扑排序的内核标记
//! `indegree` / `in_degree`。Kosaraju / Tarjan / Dijkstra / Bellman-Ford 等命名
//! 算法名在注释或变量名里出现不构成内核，但 Kahn 的入度计数是内核的硬签名，
//! 任何领域都不应自己维护一份。本门禁发现违规即失败，列出文件、行号与命中行。

use std::path::{Path, PathBuf};

/// 允许出现 Kahn 内核标记的 owner 路径（相对于仓库根）。
const OWNERS: &[&str] = &["projects/boost/boost.math/src/graph_theory"];

fn is_owner(rel: &Path) -> bool {
    let rel = rel.to_string_lossy().replace('\\', "/");
    OWNERS.iter().any(|owner| rel.starts_with(owner))
}

fn walk_sources(root: &Path, sink: &mut Vec<PathBuf>) {
    let projects = root.join("projects");
    let mut stack = vec![projects];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir)
        else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.ends_with("target") || path.ends_with(".git") {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            }
            else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                // 只扫 src/ 目录：tests/、fixtures/ 与 build.rs 里的示例代码不算内核。
                if path.components().any(|component| component.as_os_str() == "src") {
                    sink.push(path);
                }
            }
        }
    }
}

fn kernel_marker(line: &str) -> Option<&'static str> {
    let lower = line.to_ascii_lowercase();
    // 注释与字符串字面量里的 indegree 不算；但为了门禁的保守性，只要源码里出现
    // 这两个词就标记——owner 之外的任何用法都值得人工复核。
    if lower.contains("indegree") || lower.contains("in_degree") {
        return Some("in-degree counter (Kahn kernel)");
    }
    None
}

#[test]
fn no_parallel_topological_kernel_outside_owners() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let root = root.canonicalize().unwrap_or_else(|_| root.clone());
    let mut sources = Vec::new();
    walk_sources(&root, &mut sources);

    let mut violations = Vec::new();
    for path in sources {
        let rel = path.strip_prefix(&root).unwrap_or(&path);
        if is_owner(rel) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path)
        else {
            continue;
        };
        for (line_no, line) in content.lines().enumerate() {
            if let Some(marker) = kernel_marker(line) {
                violations.push(format!("{}:{}: {} — {}", rel.display(), line_no + 1, marker, line.trim()));
            }
        }
    }
    assert!(violations.is_empty(), "found parallel topological kernels outside owners:\n{}", violations.join("\n"));
}
