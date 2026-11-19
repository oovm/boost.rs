//! 过渡层：仍在等待 `data.text` 替代的 `vcc-data` 入口。
//!
//! 新代码不得在此扩展格式规则。只做 re-export 与桥接，便于逐后端迁入 `data-binary-*` / `data-text-*`。
//!
//! | 模块 | 状态 |
//! |------|------|
//! | `binary` | `data-binary-jvm` + `data-binary-pe` 已覆盖；emitter 不再直接依赖 `vcc-data` |
//! | `msil` | 经 `data-binary-pe::msil` 过渡，待 `data-text-msil` |

pub mod binary;
pub mod msil;
