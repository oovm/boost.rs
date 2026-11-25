//! Emitter 操作错误（替代 miette `Report` / `miette!`）。

/// emitter 内 `Result` 别名（与 `compiler_target::backends::BackendError` 同形）。
pub type EmitterError = String;

/// emitter 内 `Result` 别名。
pub type Result<T> = std::result::Result<T, EmitterError>;
