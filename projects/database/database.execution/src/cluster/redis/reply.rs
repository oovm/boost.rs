//! RESP 回复分帧与 `MATCH` 模式匹配。
//!
//! 命令家族共享这一小组 primitives：错误、bulk、null bulk、null array 以及
//! `KEYS` / `SCAN` 的 glob 匹配。这里只写线协议字节，不承载任何命令语义，
//! 也不持有连接状态；协议版本由调用方传入，因此 RESP2 / RESP3 的差异集中在一处。

use std::io::{self, Write};

/// 写一条错误回复，把换行折成空格以保持单行错误体。
pub fn write_error(stream: &mut dyn Write, message: &str) -> io::Result<()> {
    let sanitized =
        message.chars().map(|character| if matches!(character, '\r' | '\n') { ' ' } else { character }).take(512).collect::<String>();
    write!(stream, "-ERR {sanitized}\r\n")
}

/// 写一条 bulk string 回复。
pub fn write_bulk(stream: &mut dyn Write, bytes: &[u8]) -> io::Result<()> {
    // A collection reply calls this once per element, so the decimal length is
    // rendered from a stack buffer instead of the generic formatting machinery.
    let mut digits = [0_u8; 20];
    let mut index = digits.len();
    let mut length = bytes.len();
    loop {
        index -= 1;
        digits[index] = b'0' + (length % 10) as u8;
        length /= 10;
        if length == 0 {
            break;
        }
    }
    stream.write_all(b"$")?;
    stream.write_all(&digits[index..])?;
    stream.write_all(b"\r\n")?;
    stream.write_all(bytes)?;
    stream.write_all(b"\r\n")
}

/// 按协商版本写 null 回复。
///
/// RESP2 用缺失 bulk 的 `$-1\r\n`，RESP3 用专用 null 类型 `_\r\n`，RESP3 解析器会把
/// `$-1` 当成协议错误，因此版本必须来自连接状态而不是固定字面量。
pub fn write_null(stream: &mut dyn Write, protocol: u8) -> io::Result<()> {
    stream.write_all(if protocol == 3 { b"_\r\n" as &[u8] } else { b"$-1\r\n" })
}

/// 写 `EXEC` 在 watch 失效时使用的 nil 数组。
///
/// RESP2 是 `*-1\r\n`（null array，而不是 [`write_null`] 的 null bulk），RESP3
/// 使用专用 null 类型 `_\r\n`。
pub fn write_null_array(stream: &mut dyn Write, protocol: u8) -> io::Result<()> {
    stream.write_all(if protocol == 3 { b"_\r\n" as &[u8] } else { b"*-1\r\n" })
}

/// 未知命令的真机文案：命令名之外还要回显参数前缀，末尾带一个空格。
pub fn write_unknown_command(stream: &mut dyn Write, arguments: &[&[u8]]) -> io::Result<()> {
    let name = String::from_utf8_lossy(&arguments[0]);
    let shown: Vec<String> = arguments[1..].iter().map(|argument| format!("'{}'", String::from_utf8_lossy(argument))).collect();
    if shown.is_empty() {
        write_error(stream, &format!("unknown command '{name}'"))
    }
    else {
        write_error(stream, &format!("unknown command '{name}', with args beginning with: {} ", shown.join(" ")))
    }
}

/// 简易 Redis glob：支持 `*`、`?`、`[...]` 字符类与反斜杠转义，其余字符按字面量比较。
///
/// 字符类支持 `[^...]` 取反、`a-z` 区间与类内 `\` 转义，大小写敏感，与真机
/// `stringmatchlen` 的匹配口径一致。`MATCH` 只影响本次返回的键，不改变游标推进。
pub fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut pattern_index, mut text_index) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while text_index < text.len() {
        if pattern_index < pattern.len() {
            match pattern[pattern_index] {
                b'*' => {
                    star = Some((pattern_index, text_index));
                    pattern_index += 1;
                    continue;
                }
                b'?' => {
                    pattern_index += 1;
                    text_index += 1;
                    continue;
                }
                b'[' => {
                    if let Some((length, matched)) = class_match(&pattern[pattern_index..], text[text_index]) {
                        if matched {
                            pattern_index += length;
                            text_index += 1;
                            continue;
                        }
                    }
                }
                b'\\' if pattern_index + 1 < pattern.len() && pattern[pattern_index + 1] == text[text_index] => {
                    pattern_index += 2;
                    text_index += 1;
                    continue;
                }
                byte if byte == text[text_index] => {
                    pattern_index += 1;
                    text_index += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some((star_index, resume)) => {
                pattern_index = star_index + 1;
                text_index = resume + 1;
                star = Some((star_index, text_index));
            }
            None => return false,
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

/// 匹配一个以 `[` 开头的字符类，返回消费的图案字节数与是否命中。
///
/// 口径对齐真机 `stringmatchlen`：`^` 只在紧跟 `[` 时取反，`\` 转义类内下一个字节，
/// `a-z` 区间起点大于终点时自动交换，未闭合的字符类在模式末尾自动闭合。
fn class_match(pattern: &[u8], byte: u8) -> Option<(usize, bool)> {
    let mut index = 1usize;
    let negated = pattern.get(index) == Some(&b'^');
    if negated {
        index += 1;
    }
    let mut matched = false;
    while index < pattern.len() {
        match pattern[index] {
            b']' => {
                index += 1;
                break;
            }
            b'\\' if index + 1 < pattern.len() => {
                if pattern[index + 1] == byte {
                    matched = true;
                }
                index += 2;
            }
            start if index + 2 < pattern.len() && pattern[index + 1] == b'-' => {
                let (mut low, mut high) = (start, pattern[index + 2]);
                if low > high {
                    std::mem::swap(&mut low, &mut high);
                }
                if byte >= low && byte <= high {
                    matched = true;
                }
                index += 3;
            }
            literal => {
                if literal == byte {
                    matched = true;
                }
                index += 1;
            }
        }
    }
    // 与 `Redis` 一致：未闭合的字符类在模式末尾自动闭合，`[1` 仍可匹配 `1`。
    Some((index, if negated { !matched } else { matched }))
}
