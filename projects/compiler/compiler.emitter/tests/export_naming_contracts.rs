//! 导出命名合同：语言侧冻结源码名，各目标所有者按自己的命名合同投影。
//!
//! 机器码 lane（Node / CPython / 普通共享库）与 wasm lane 的投影、碰撞拒绝与
//! 导出公开名回退合同已随纵链迁到语言侧 `language.valkyrie` 的对应管道；
//! 本文件保留 CPython 模块名归一化纯函数。

use compiler_target::cpython_module_name;

#[test]
fn cpython_module_name_normalizes_project() {
    // 工程名保留仓库身份，模块名是确定性的合法标识符：文件名、`PyInit_`、
    // 清单与宿主 `import` 必须共用同一个冻结值。
    assert_eq!(cpython_module_name("leetcode-multiply-strings").unwrap(), "leetcode_multiply_strings");
    assert_eq!(cpython_module_name("app").unwrap(), "app");
    assert_eq!(cpython_module_name("9lives").unwrap(), "_9lives");
    assert!(cpython_module_name("").is_err(), "空工程名无法归一化成合法模块名");
}

// wasm lane 的入口符号合同（`main` / `_start` 由宿主边界冻结、导出公开名冲突与
// 重复拒绝）已随纵链迁到语言侧 `language.valkyrie` 的 `pipeline/wasm/`，等价覆盖
// 见其 `wasm_lane_tests`。
