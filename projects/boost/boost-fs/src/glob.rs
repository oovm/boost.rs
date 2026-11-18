//! 路径 glob 匹配与按模式枚举文件。
//!
//! 冻结的语义（不随调用方猜测）：
//!
//! - 分隔符统一为 `/`；模式与路径里的 `\` 都按 `/` 处理。
//! - `*` 匹配组件内零个或多个字符，可以为空，也会匹配以 `.` 开头的名字。
//! - `?` 匹配组件内恰好一个字符。
//! - `**` 只有独占一个组件时才是递归段，匹配零层或多层目录；写在其它字符之间
//!   时按普通 `*` 处理。作为最后一个组件时只枚举目录，不枚举文件。
//! - 区分大小写，跟随宿主文件系统，不做平台特判。
//! - 不支持字符类 `[...]`、转义与花括号；出现即返回 [`PatternError::Unsupported`]，
//!   不会静默按字面量匹配。
//! - 模式按 UTF-8 解释。非 UTF-8 的路径组件永远不满足通配符，只有与字面模式
//!   组件完全相同（`OsStr` 逐字相等）时才算命中。
//! - 相对模式以进程当前工作目录为基准，绝对模式从根或盘符开始。
//! - 遍历按目录名排序进行，不可读的目录跳过（与 `glob` crate 迭代器忽略单个
//!   条目错误的既有行为一致），因此结果只包含当前确实存在的路径。

use core::fmt::{self, Display, Formatter};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

/// 模式或匹配参数的错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternError {
    /// 模式为空。
    Empty,
    /// 使用了本模块未冻结的语法（如字符类或花括号）。
    Unsupported {
        /// 触发拒绝的字符。
        character: char,
    },
}

impl Display for PatternError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the glob pattern is empty"),
            Self::Unsupported { character } => {
                write!(f, "the glob pattern uses unsupported syntax starting at `{character}`")
            }
        }
    }
}

impl std::error::Error for PatternError {}

#[derive(Clone, Debug)]
enum Segment {
    Literal(String),
    Wildcard(String),
    Recursive,
}

/// 已解析的 glob 模式：固定的字面前缀加待匹配的组件。
#[derive(Clone, Debug)]
pub struct Pattern {
    base: PathBuf,
    segments: Vec<Segment>,
}

impl Pattern {
    /// 解析一个 glob 模式；语法不在冻结范围内时返回错误。
    pub fn new(pattern: &str) -> Result<Self, PatternError> {
        if pattern.is_empty() {
            return Err(PatternError::Empty);
        }
        reject_unsupported(pattern)?;

        let normalized = pattern.replace('\\', "/");
        let mut parts = normalized.split('/').peekable();
        let mut base = PathBuf::new();

        if normalized.starts_with('/') {
            base.push("/");
            parts.next();
        }
        else if let Some(first) = parts.peek() {
            if first.len() == 2 && first.ends_with(':') {
                base.push(format!("{first}/"));
                parts.next();
            }
        }

        let mut segments = Vec::new();
        for part in parts {
            if part.is_empty() || part == "." {
                continue;
            }
            let wildcard = part.contains('*') || part.contains('?');
            if !wildcard && segments.is_empty() {
                base.push(part);
                continue;
            }
            segments.push(if part == "**" {
                Segment::Recursive
            }
            else if wildcard {
                Segment::Wildcard(part.to_owned())
            }
            else {
                Segment::Literal(part.to_owned())
            });
        }

        if base.as_os_str().is_empty() {
            base.push(".");
        }
        Ok(Self { base, segments })
    }

    /// 模式是否只是字面路径，不含任何通配符。
    #[must_use]
    pub fn is_literal(&self) -> bool {
        self.segments.is_empty()
    }

    /// 判断一个具体路径是否匹配本模式。
    ///
    /// 只做组件匹配，不访问文件系统；路径是否存在由调用方自行判断。
    #[must_use]
    pub fn matches(&self, path: &Path) -> bool {
        let Ok(rest) = path.strip_prefix(&self.base) else {
            return false;
        };
        let names: Vec<&OsStr> =
            rest.components().filter_map(|component| match component {
                std::path::Component::Normal(name) => Some(name),
                _ => None,
            }).collect();
        match_segments(&self.segments, &names)
    }
}

/// 按模式枚举当前存在的路径；结果按字典序排序并去重。
///
/// 模式语法错误返回 [`PatternError`]；遍历中不可读的目录与条目跳过。
pub fn glob(pattern: &str) -> Result<Vec<PathBuf>, PatternError> {
    let pattern = Pattern::new(pattern)?;
    let mut matches = Vec::new();
    expand(&pattern.base, &pattern.segments, &mut matches);
    if pattern.base == Path::new(".") {
        matches = matches.into_iter().map(|path| path.strip_prefix(".").map(Path::to_owned).unwrap_or(path)).collect();
    }
    matches.sort();
    matches.dedup();
    Ok(matches)
}

fn reject_unsupported(pattern: &str) -> Result<(), PatternError> {
    if let Some(character) = pattern.chars().find(|character| matches!(character, '[' | ']' | '{' | '}')) {
        return Err(PatternError::Unsupported { character });
    }
    Ok(())
}

fn expand(directory: &Path, segments: &[Segment], matches: &mut Vec<PathBuf>) {
    let Some((first, rest)) = segments.split_first() else {
        if directory.exists() {
            matches.push(directory.to_owned());
        }
        return;
    };

    match first {
        Segment::Recursive => {
            // 零层目录，以及任意深度的子目录。
            expand(directory, rest, matches);
            for entry in sorted_entries(directory) {
                if entry.is_dir() {
                    expand(&entry, segments, matches);
                }
            }
        }
        Segment::Literal(name) => {
            let candidate = directory.join(name);
            if rest.is_empty() {
                if candidate.exists() {
                    matches.push(candidate);
                }
            }
            else if candidate.is_dir() {
                expand(&candidate, rest, matches);
            }
        }
        Segment::Wildcard(mask) => {
            for entry in sorted_entries(directory) {
                let Some(name) = entry.file_name().and_then(OsStr::to_str) else {
                    continue;
                };
                if !wildcard_match(mask, name) {
                    continue;
                }
                if rest.is_empty() {
                    matches.push(entry);
                }
                else if entry.is_dir() {
                    expand(&entry, rest, matches);
                }
            }
        }
    }
}

fn sorted_entries(directory: &Path) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut entries: Vec<PathBuf> = read.filter_map(|entry| entry.ok().map(|entry| entry.path())).collect();
    entries.sort();
    entries
}

fn match_segments(segments: &[Segment], names: &[&OsStr]) -> bool {
    match segments.split_first() {
        None => names.is_empty(),
        Some((Segment::Recursive, rest)) => (0..=names.len()).any(|skip| match_segments(rest, &names[skip..])),
        Some((Segment::Literal(expected), rest)) => {
            names.first().is_some_and(|name| *name == OsStr::new(expected)) && match_segments(rest, &names[1..])
        }
        Some((Segment::Wildcard(mask), rest)) => {
            names
                .first()
                .and_then(|name| name.to_str())
                .is_some_and(|name| wildcard_match(mask, name))
                && match_segments(rest, &names[1..])
        }
    }
}

/// 组件内通配：`*` 任意长度，`?` 单个字符，使用回溯而不是递归展开。
fn wildcard_match(mask: &str, name: &str) -> bool {
    let mask: Vec<char> = mask.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut m, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;

    while n < name.len() {
        if m < mask.len() && (mask[m] == '?' || mask[m] == name[n]) {
            m += 1;
            n += 1;
        }
        else if m < mask.len() && mask[m] == '*' {
            star = Some((m, n));
            m += 1;
        }
        else if let Some((star_mask, star_name)) = star {
            m = star_mask + 1;
            n = star_name + 1;
            star = Some((star_mask, star_name + 1));
        }
        else {
            return false;
        }
    }

    mask[m..].iter().all(|character| *character == '*')
}
