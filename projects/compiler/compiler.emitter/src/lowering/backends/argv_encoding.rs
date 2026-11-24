//! argv → 语言标量的唯一编码合同。
//!
//! managed 后端（JVM `class` / CLR `MSIL`）可以为导出入口合成 argv launcher，但宿主
//! 命令行只提供文本。本模块是「argv 文本 → 入口标量载体」编码的**唯一**判定表：
//! 两个后端只负责把入口事实投影成 [`ManagedAbiEntry`] 的载体逻辑名
//! （`i32` / `u8` / `f32` / `string` …），随后机械消费 [`argv_encoding`]；任何一端
//! 都不得再写第二份可表达性规则或第二套解析语法。
//!
//! 入口事实与 argv 包装能力分离：被本表拒绝的类型仍是合法库入口，只是不生成
//! launcher，调用方不得因此让整个 lowering 失败。
//!
//! [`ManagedAbiEntry`]: crate::managed_abi::ManagedAbiEntry
//!
//! # 冻结表
//!
//! | 载体逻辑名 | argv launcher | 宿主解析契约 |
//! | --- | --- | --- |
//! | `void` | 仅作返回 | 不解析、不输出 |
//! | `bool` | 是 | 只接受 `true` / `false`（ASCII 大小写不敏感，忽略首尾空白），其它串必须宿主可见地失败 |
//! | `i32` / `i64` | 是 | 有符号十进制整型，宿主有符号解析器，溢出即失败 |
//! | `u8` / `u16` | 是 | 更宽的有符号宿主解析器 + 由本表给出的显式范围检查 `0..=255` / `0..=65535`；越界必须抛异常（宿主非零退出），不得截断或回绕 |
//! | `u32` / `u64` | 是 | 宿主无符号解析器（`parseUnsignedInt` / `parseUnsignedLong` / `UInt32.Parse` / `UInt64.Parse`）；解析器宽度与语义宽度一致，越界即失败 |
//! | `f32` / `f64` | 是 | 宿主 IEEE-754 二进制浮点解析器；`f32` 必须落到真实 `float` 载体，不得用 `f64` 顶替 |
//! | `char` | 否 | 码位 / UTF-16 code unit 的编码合同未冻结：argv 文本到字符的映射不唯一 |
//! | `string` | 否 | 语言载体是字节序列（utf8/utf16），argv 是文本，二者之间的编码合同未冻结 |
//! | `T[]` | 否 | argv 没有元素分隔与长度合同 |
//! | `i8` / `i16` | 否 | 窄有符号整型尚未纳入本轮 argv 最小集（宿主有精确解析器，但合同未冻结） |
//! | `i128` / `u128` / `f128` | 否 | 宽于宿主标量（最宽 64 位），没有对应解析器 |
//!
//! # 不变式
//!
//! - 无符号语义不得用有符号解析器冒充；范围上界由 [`ArgvEncoding::unsigned_max`]
//!   唯一给出，后端不得各自硬编码。
//! - 解析失败必须是宿主可见失败：JVM 抛未捕获异常，CLR 让 `Parse` 的
//!   `FormatException` / `OverflowException` 自然传播。
//! - `bool` fail-closed：不接受「非 `true` 即 `false`」的宽松解析。

use crate::Result;

/// argv 可表达载体的语义类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArgvKind {
    /// `void`：只允许作为返回值。
    Void,
    /// 严格布尔。
    Bool,
    /// 有符号整型，`bits` 为语义位宽。
    Signed {
        /// 语义位宽。
        bits: u8,
    },
    /// 无符号整型，`bits` 为语义位宽。
    Unsigned {
        /// 语义位宽。
        bits: u8,
    },
    /// IEEE-754 浮点，`bits` 为语义位宽。
    Float {
        /// 语义位宽。
        bits: u8,
    },
}

/// 冻结的 argv 编码条目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArgvEncoding {
    /// 载体语义类别。
    pub(crate) kind: ArgvKind,
    /// 无符号整型的包含上界；其它类别为 `None`。
    pub(crate) unsigned_max: Option<u128>,
}

const fn entry(kind: ArgvKind, unsigned_max: Option<u128>) -> ArgvEncoding {
    ArgvEncoding { kind, unsigned_max }
}

/// 唯一匹配函数：载体逻辑名 → 冻结编码；被拒绝的载体在此给出真实缺口。
pub(crate) fn argv_encoding(logical_type: &str) -> Result<ArgvEncoding> {
    Ok(match logical_type {
        "void" => entry(ArgvKind::Void, None),
        "bool" => entry(ArgvKind::Bool, None),
        "i32" => entry(ArgvKind::Signed { bits: 32 }, None),
        "i64" => entry(ArgvKind::Signed { bits: 64 }, None),
        "u8" => entry(ArgvKind::Unsigned { bits: 8 }, Some(u8::MAX as u128)),
        "u16" => entry(ArgvKind::Unsigned { bits: 16 }, Some(u16::MAX as u128)),
        "u32" => entry(ArgvKind::Unsigned { bits: 32 }, Some(u32::MAX as u128)),
        "u64" => entry(ArgvKind::Unsigned { bits: 64 }, Some(u64::MAX as u128)),
        "f32" => entry(ArgvKind::Float { bits: 32 }, None),
        "f64" => entry(ArgvKind::Float { bits: 64 }, None),
        "char" => {
            return Err("argv launcher 不支持 `char`：码位 / UTF-16 code unit 的编码合同未冻结，argv 文本到字符的映射不唯一".to_string());
        }
        "string" => {
            return Err(
                "argv launcher 不支持 `string`：语言载体是字节序列（utf8/utf16），argv 是文本，二者之间的编码合同未冻结".to_string()
            );
        }
        "i8" | "i16" => {
            return Err(format!("argv launcher 暂不支持窄有符号整型 `{logical_type}`：本轮 argv 合同最小集不含 i8/i16"));
        }
        "i128" | "u128" | "f128" => {
            return Err(format!("argv launcher 不支持 `{logical_type}`：宽于宿主标量（最宽 64 位），没有对应解析器"));
        }
        array if array.ends_with("[]") => {
            return Err(format!("argv launcher 不支持数组 `{array}`：argv 没有元素分隔与长度合同"));
        }
        other => {
            return Err(format!("argv launcher 不支持 `{other}`：argv 没有该载体的编组合同"));
        }
    })
}
