//! SQLite cell value mechanisms: storage-class ordering, truthiness, numeric
//! views, coercions, text forms and `GLOB` / `LIKE` pattern matching.

use std::{cmp::Ordering, collections::HashSet};

use database_query::sqlite::{
    bind::SortDirection,
    frontend::{BinaryOp, CastType, CompareOp},
    value::{Cell, ProbeKey, probe_key},
};
use database_storage::sqlite::Collation;

use super::eval::SortCompare;
/// SQLite storage-class order: `NULL` < numbers < text < blob.
pub(crate) fn storage_rank(cell: &Cell) -> u8 {
    match cell {
        Cell::Null => 0,
        Cell::Integer(_) | Cell::Real(_) => 1,
        Cell::Text(_) => 2,
        Cell::Blob(_) => 3,
    }
}

/// Compares two cells with the SQLite ordering rules.
///
/// Integers and reals compare numerically across the two storage classes, text
/// compares with `BINARY`, and blobs compare byte by byte.
pub fn compare_cells(left: &Cell, right: &Cell) -> Ordering {
    compare_cells_with(left, right, Collation::Binary)
}

/// 按给定排序规则比较两个单元格，文本以外的比较不受排序规则影响。
pub fn compare_cells_with(left: &Cell, right: &Cell, collation: Collation) -> Ordering {
    use Ordering::Equal;
    let (left_rank, right_rank) = (storage_rank(left), storage_rank(right));
    if left_rank != right_rank {
        return left_rank.cmp(&right_rank);
    }
    match (left, right) {
        (Cell::Null, Cell::Null) => Equal,
        (Cell::Integer(left), Cell::Integer(right)) => left.cmp(right),
        (Cell::Real(left), Cell::Real(right)) => left.partial_cmp(right).unwrap_or(Equal),
        (Cell::Integer(left), Cell::Real(right)) => (*left as f64).partial_cmp(right).unwrap_or(Equal),
        (Cell::Real(left), Cell::Integer(right)) => left.partial_cmp(&(*right as f64)).unwrap_or(Equal),
        (Cell::Text(left), Cell::Text(right)) => collation.compare_text(left, right),
        (Cell::Blob(left), Cell::Blob(right)) => left.cmp(right),
        _ => Equal,
    }
}

/// Applies one comparison operator. `None` means the result is SQL `NULL`.
pub fn compare_values(left: &Cell, right: &Cell, operator: CompareOp, collation: Collation) -> Option<bool> {
    use Ordering::{Equal, Greater, Less};
    if matches!(left, Cell::Null) || matches!(right, Cell::Null) {
        return None;
    }
    let ordering = compare_cells_with(left, right, collation);
    Some(match operator {
        CompareOp::Eq => ordering == Equal,
        CompareOp::NotEq => ordering != Equal,
        CompareOp::Lt => ordering == Less,
        CompareOp::LtEq => ordering != Greater,
        CompareOp::Gt => ordering == Greater,
        CompareOp::GtEq => ordering != Less,
    })
}

/// SQLite boolean conversion: `NULL` stays unknown, numbers are true when
/// non-zero, and text or blobs are converted numerically first (`'1'` and
/// `x'31'` are true, `'abc'` and `x'01'` are false).
pub fn truthiness(cell: &Cell) -> Option<bool> {
    match cell {
        Cell::Null => None,
        Cell::Integer(value) => Some(*value != 0),
        Cell::Real(value) => Some(*value != 0.0),
        Cell::Text(text) => Some(number_truthy(&text_number(text))),
        Cell::Blob(bytes) => Some(number_truthy(&text_number(&String::from_utf8_lossy(bytes)))),
    }
}

/// Compares two composite window sort keys, honouring each column's direction.
pub fn compare_ordered(left: &[Cell], right: &[Cell], directions: &[SortDirection]) -> Ordering {
    for (index, direction) in directions.iter().enumerate() {
        let ordering = direction.compare(&left[index], &right[index]);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// Numeric view of a cell used by arithmetic and the numeric functions.
#[derive(Clone, Copy)]
pub enum Number {
    /// 64-bit integer.
    Int(i64),
    /// Binary64 value.
    Real(f64),
}

/// SQLite numeric coercion: `NULL` has no numeric value, and text or blobs
/// contribute their longest numeric prefix (unparseable input becomes zero).
pub fn numeric(cell: &Cell) -> Option<Number> {
    match cell {
        Cell::Null => None,
        Cell::Integer(value) => Some(Number::Int(*value)),
        Cell::Real(value) => Some(Number::Real(*value)),
        Cell::Text(text) => Some(text_number(text)),
        // Blobs convert through their text form too, so `x'31' + 1` is 2.
        Cell::Blob(bytes) => Some(text_number(&String::from_utf8_lossy(bytes))),
    }
}

/// Numeric value of text: SQLite takes the longest numeric *prefix*
/// (`sqlite3AtoF`), so `'2abc'` is 2 and `'abc'` is 0.
pub(crate) fn text_number(text: &str) -> Number {
    let trimmed = text.trim_start();
    let bytes = trimmed.as_bytes();
    let mut index = 0;
    if matches!(bytes.first(), Some(b'+') | Some(b'-')) {
        index = 1;
    }
    let integer_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    let mut has_digits = index > integer_start;
    let mut is_real = false;
    if index < bytes.len() && bytes[index] == b'.' {
        is_real = true;
        index += 1;
        let fraction_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        has_digits |= index > fraction_start;
    }
    if !has_digits {
        return Number::Int(0);
    }
    // An exponent only counts when it is followed by at least one digit;
    // otherwise the number ends at the mantissa (`'1e'` is 1).
    let mut end = index;
    if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
        let mut probe = index + 1;
        if probe < bytes.len() && matches!(bytes[probe], b'+' | b'-') {
            probe += 1;
        }
        let exponent_start = probe;
        while probe < bytes.len() && bytes[probe].is_ascii_digit() {
            probe += 1;
        }
        if probe > exponent_start {
            end = probe;
            is_real = true;
        }
    }
    let prefix = &trimmed[..end];
    if is_real {
        prefix.parse::<f64>().map(Number::Real).unwrap_or(Number::Int(0))
    }
    else {
        prefix.parse::<i64>().map(Number::Int).unwrap_or_else(|_| prefix.parse::<f64>().map(Number::Real).unwrap_or(Number::Int(0)))
    }
}

/// `CAST(expr AS type)`: SQLite converts between its five storage classes and
/// keeps `NULL` unchanged.
pub(crate) fn cast_cell(cell: &Cell, target: CastType) -> Cell {
    if matches!(cell, Cell::Null) {
        return Cell::Null;
    }
    match target {
        CastType::Integer => Cell::Integer(cast_integer(cell)),
        CastType::Real => Cell::Real(cast_real(cell)),
        CastType::Text => Cell::Text(cast_text(cell)),
        CastType::Blob => Cell::Blob(cast_text(cell).into_bytes()),
        CastType::Numeric => cast_numeric(cell),
    }
}

pub(crate) fn cast_integer(cell: &Cell) -> i64 {
    match cell {
        Cell::Integer(value) => *value,
        // SQLite truncates toward zero and saturates at the `i64` bounds.
        Cell::Real(value) => *value as i64,
        Cell::Text(text) => integer_prefix(text),
        // Blobs convert through their text form, so `cast(x'31' as integer)` is 1.
        Cell::Blob(bytes) => integer_prefix(&String::from_utf8_lossy(bytes)),
        Cell::Null => 0,
    }
}

/// Parses the leading integer prefix of `text`, stopping at the first
/// non-digit the way `sqlite3Atoi64` does. `'1e3'` therefore becomes `1`.
pub(crate) fn integer_prefix(text: &str) -> i64 {
    let trimmed = text.trim_start();
    let bytes = trimmed.as_bytes();
    let mut index = 0;
    let negative = match bytes.first() {
        Some(b'+') => {
            index = 1;
            false
        }
        Some(b'-') => {
            index = 1;
            true
        }
        _ => false,
    };
    let start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index == start {
        return 0;
    }
    let magnitude = trimmed[start..index].parse::<i128>().unwrap_or(i128::MAX);
    let signed = if negative { -magnitude } else { magnitude };
    signed.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

pub(crate) fn cast_real(cell: &Cell) -> f64 {
    match cell {
        Cell::Integer(value) => *value as f64,
        Cell::Real(value) => *value,
        Cell::Text(text) => real_prefix(text),
        Cell::Blob(bytes) => real_prefix(&String::from_utf8_lossy(bytes)),
        Cell::Null => 0.0,
    }
}

/// Parses the leading real prefix of `text`, stopping at the first byte that
/// cannot be part of a number, the way `sqlite3AtoF` does.
pub(crate) fn real_prefix(text: &str) -> f64 {
    let trimmed = text.trim_start();
    let bytes = trimmed.as_bytes();
    let mut index = 0;
    if matches!(bytes.first(), Some(b'+') | Some(b'-')) {
        index = 1;
    }
    let start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    let mut has_dot = false;
    if index < bytes.len() && bytes[index] == b'.' {
        has_dot = true;
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
    }
    if index == start && !has_dot {
        return 0.0;
    }
    let mut end = index;
    if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
        let mut cursor = index + 1;
        if matches!(bytes.get(cursor), Some(b'+') | Some(b'-')) {
            cursor += 1;
        }
        let exponent_start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor > exponent_start {
            end = cursor;
        }
    }
    trimmed[..end].parse::<f64>().unwrap_or(0.0)
}

pub(crate) fn cast_text(cell: &Cell) -> String {
    match cell {
        Cell::Null => String::new(),
        Cell::Integer(value) => value.to_string(),
        Cell::Real(value) => sqlite_real_text(*value),
        Cell::Text(text) => text.clone(),
        Cell::Blob(bytes) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

pub(crate) fn cast_numeric(cell: &Cell) -> Cell {
    match cell {
        Cell::Integer(value) => Cell::Integer(*value),
        Cell::Real(value) => Cell::Real(*value),
        Cell::Text(text) => numeric_text(text),
        Cell::Blob(bytes) => numeric_text(&String::from_utf8_lossy(bytes)),
        Cell::Null => Cell::Integer(0),
    }
}

/// Numeric view of text for `CAST(... AS NUMERIC)`: the same prefix rule as
/// arithmetic, collapsed back to an integer when the value is integral.
pub(crate) fn numeric_text(text: &str) -> Cell {
    match text_number(text) {
        Number::Int(value) => Cell::Integer(value),
        Number::Real(value) if value.is_finite() && value.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&value) => {
            Cell::Integer(value as i64)
        }
        Number::Real(value) => Cell::Real(value),
    }
}

/// Text form shared by `LIKE` and `||`; `NULL` has none and short-circuits.
pub fn text_form(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Null => None,
        Cell::Integer(value) => Some(value.to_string()),
        Cell::Real(value) => Some(sqlite_real_text(*value)),
        Cell::Text(text) => Some(text.clone()),
        Cell::Blob(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
    }
}

/// `||`: concatenates the text forms of both sides. `NULL` propagates and the
/// result is always `TEXT`, even when both operands are numbers.
pub(crate) fn concat(left: &Cell, right: &Cell) -> Cell {
    match (text_form(left), text_form(right)) {
        (Some(left), Some(right)) => Cell::Text(format!("{left}{right}")),
        _ => Cell::Null,
    }
}

/// Renders a real the way SQLite does with `%!.15g`: 15 significant digits,
/// always a decimal point, and an exponent only outside `[1e-4, 1e15)`.
pub(crate) fn sqlite_real_text(value: f64) -> String {
    if value == 0.0 {
        return "0.0".to_string();
    }
    // `{:.14e}` carries 15 significant digits and always prints an exponent.
    let scientific = format!("{value:.14e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((scientific.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|character| character.is_ascii_digit()).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let mut text = if exponent < -4 || exponent >= 15 {
        let (head, tail) = digits.split_at(1);
        let tail = if tail.is_empty() { "0" } else { tail };
        format!("{head}.{tail}e{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.abs())
    }
    else {
        let point = exponent + 1;
        if point <= 0 {
            format!("0.{}{digits}", "0".repeat((-point) as usize))
        }
        else if point as usize >= digits.len() {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        }
        else {
            format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
        }
    };
    if negative {
        text.insert(0, '-');
    }
    text
}

/// Matches one `GLOB` pattern against a value, one character at a time.
///
/// `*` matches any run including the empty one, `?` matches exactly one
/// character, and `[...]` is a class: `^` negates it, `a-c` is an inclusive
/// range, a leading `]` is a literal member, and an unclosed `[` never matches.
/// Unlike `LIKE`, every comparison is case sensitive.
pub(crate) fn glob_match(value: &[char], pattern: &[char]) -> bool {
    let Some((first, rest)) = pattern.split_first()
    else {
        return value.is_empty();
    };
    match *first {
        '*' => glob_match(value, rest) || value.split_first().is_some_and(|(_, tail)| glob_match(tail, pattern)),
        '?' => value.split_first().is_some_and(|(_, tail)| glob_match(tail, rest)),
        '[' => {
            let Some((candidate, tail)) = value.split_first()
            else {
                return false;
            };
            match glob_class(*candidate, rest) {
                Some(true) => glob_match(tail, &rest[glob_class_width(rest)..]),
                _ => false,
            }
        }
        literal => value.split_first().is_some_and(|(candidate, tail)| *candidate == literal && glob_match(tail, rest)),
    }
}

/// Width of the `[...]` class that starts at `pattern[0]`, including both
/// brackets. Zero means the class is never closed.
pub(crate) fn glob_class_width(pattern: &[char]) -> usize {
    let mut index = usize::from(pattern.first() == Some(&'^'));
    let mut first_item = true;
    while index < pattern.len() {
        if pattern[index] == ']' && !first_item {
            return index + 1;
        }
        first_item = false;
        // `a-c` is a range only when a member follows the dash.
        if index + 2 < pattern.len() && pattern[index + 1] == '-' && pattern[index + 2] != ']' {
            index += 3;
        }
        else {
            index += 1;
        }
    }
    0
}

/// Matches one character against the class body that follows `[`.
pub(crate) fn glob_class(candidate: char, pattern: &[char]) -> Option<bool> {
    let width = glob_class_width(pattern);
    if width == 0 {
        return None;
    }
    let invert = pattern.first() == Some(&'^');
    let body = &pattern[usize::from(invert)..width - 1];
    let mut matched = false;
    let mut index = 0;
    let mut first_item = true;
    while index < body.len() {
        // A leading `]` is a member instead of the closing bracket.
        if body[index] == ']' && !first_item {
            break;
        }
        first_item = false;
        if index + 2 < body.len() && body[index + 1] == '-' && body[index + 2] != ']' {
            if candidate >= body[index] && candidate <= body[index + 2] {
                matched = true;
            }
            index += 3;
        }
        else {
            if candidate == body[index] {
                matched = true;
            }
            index += 1;
        }
    }
    Some(if invert { !matched } else { matched })
}

/// `LIKE` folds case only for ASCII letters; every other character compares
/// exactly, which is why `Ü` never matches `ü`.
pub(crate) fn like_eq(left: char, right: char) -> bool {
    if left.is_ascii() && right.is_ascii() { left.eq_ignore_ascii_case(&right) } else { left == right }
}

/// Matches one `LIKE` pattern against a value, one character at a time.
///
/// `%` matches any run including the empty one, `_` matches exactly one
/// character, and the `ESCAPE` character turns the next pattern character into
/// a literal.
pub(crate) fn like_match(value: &[char], pattern: &[char], escape: Option<char>) -> bool {
    let Some((first, rest)) = pattern.split_first()
    else {
        return value.is_empty();
    };
    if escape != Some(*first) && *first == '%' {
        return like_match(value, rest, escape) || value.split_first().is_some_and(|(_, tail)| like_match(tail, pattern, escape));
    }
    let literal = if escape == Some(*first) {
        // A trailing escape character has nothing left to quote.
        match rest.split_first() {
            Some((quoted, rest)) => (*quoted, rest),
            None => return false,
        }
    }
    else {
        (*first, rest)
    };
    let Some((candidate, tail)) = value.split_first()
    else {
        return false;
    };
    if escape != Some(*first) && *first == '_' {
        return like_match(tail, literal.1, escape);
    }
    like_eq(literal.0, *candidate) && like_match(tail, literal.1, escape)
}

/// One consumed pattern element.
enum LikeElement {
    /// The pattern ended.
    End,
    /// `_`: exactly one character.
    Any,
    /// A literal character, with any `ESCAPE` prefix already consumed.
    Literal(char),
}

/// 纯前缀模式的快速形态:模式是 `<prefix>%` 或 `<prefix>`,且 `prefix` 内没有
/// `%`、`_` 与转义字符。返回 `prefix` 与「是否允许更长」。
///
/// 命中时 `LIKE` 等价于一次前缀比较,不必启动逐字符匹配器。带 `ESCAPE` 的
/// 模式保持原匹配器:转义字符自身也要参与判定。
pub(crate) fn like_plain_prefix(pattern: &str, escape: Option<char>) -> Option<(&str, bool)> {
    if escape.is_some() {
        return None;
    }
    let (prefix, open) = match pattern.strip_suffix('%') {
        Some(prefix) => (prefix, true),
        None => (pattern, false),
    };
    if prefix.contains(['%', '_']) {
        return None;
    }
    Some((prefix, open))
}

/// `LIKE` 前缀匹配:`value` 是否以 `prefix` 开头(`open` 为假时要求整串相等),
/// ASCII 字母不区分大小写,与 [`like_eq`] 的逐字符规则一致。
///
/// UTF-8 是自同步编码,逐字节做 ASCII 折叠比较与前缀字符比较等价:多字节字符
/// 必须逐字节相同,ASCII 字节也不会出现在多字节序列内部。
pub(crate) fn like_prefix_text(value: &str, prefix: &str, open: bool) -> bool {
    let value = value.as_bytes();
    let prefix = prefix.as_bytes();
    let matched_length = if open { value.len() >= prefix.len() } else { value.len() == prefix.len() };
    matched_length && prefix.iter().zip(&value[..prefix.len()]).all(|(pattern, candidate)| pattern.eq_ignore_ascii_case(candidate))
}

/// Matches one `LIKE` pattern against one text value without allocating.
///
/// Same semantics as [`like_match`] — `%` matches any run including the empty
/// one, `_` exactly one character, and the `ESCAPE` character quotes the next
/// pattern character — but it walks `Chars` clones and remembers the last `%`
/// as a backtracking point instead of materializing two `Vec<char>` per call.
/// `Chars` is a pair of pointers, so cloning one is free.
pub(crate) fn like_match_text(value: &str, pattern: &str, escape: Option<char>) -> bool {
    let mut value: std::str::Chars<'_> = value.chars();
    let mut pattern: std::str::Chars<'_> = pattern.chars();
    // Pattern position right after the most recent `%`, plus the value
    // position that `%` started at; together they are the retry point.
    let mut retry_pattern: Option<std::str::Chars<'_>> = None;
    let mut retry_value: Option<std::str::Chars<'_>> = None;
    loop {
        let mut rest = pattern.clone();
        let element = match rest.next() {
            Some(first) if escape != Some(first) && first == '%' => {
                retry_pattern = Some(rest.clone());
                retry_value = Some(value.clone());
                pattern = rest;
                continue;
            }
            Some(first) if escape == Some(first) => match rest.next() {
                Some(quoted) => {
                    pattern = rest;
                    LikeElement::Literal(quoted)
                }
                // A trailing escape character has nothing left to quote.
                None => return false,
            },
            Some('_') => {
                pattern = rest;
                LikeElement::Any
            }
            Some(first) => {
                pattern = rest;
                LikeElement::Literal(first)
            }
            None => LikeElement::End,
        };
        let matched = match element {
            LikeElement::End => {
                if value.clone().next().is_none() {
                    return true;
                }
                // Extend the run the last `%` swallowed by one character and
                // retry the (already exhausted) pattern against the shorter tail.
                let Some(next_value) = retry_value.as_mut()
                else {
                    return false;
                };
                let Some(next_pattern) = retry_pattern.as_ref()
                else {
                    return false;
                };
                if next_value.next().is_none() {
                    return false;
                }
                value = next_value.clone();
                pattern = next_pattern.clone();
                continue;
            }
            LikeElement::Any => value.next().is_some(),
            LikeElement::Literal(literal) => matches!(value.next(), Some(candidate) if like_eq(literal, candidate)),
        };
        if matched {
            continue;
        }
        // Extend the run the last `%` swallowed by one character and retry.
        let Some(next_value) = retry_value.as_mut()
        else {
            return false;
        };
        let Some(next_pattern) = retry_pattern.as_ref()
        else {
            return false;
        };
        if next_value.next().is_none() {
            return false;
        }
        value = next_value.clone();
        pattern = next_pattern.clone();
    }
}

pub(crate) fn number_truthy(number: &Number) -> bool {
    match number {
        Number::Int(value) => *value != 0,
        Number::Real(value) => *value != 0.0,
    }
}

/// The binary floating-point view of a numeric value.
pub fn number_f64(number: Number) -> f64 {
    match number {
        Number::Int(value) => value as f64,
        Number::Real(value) => value,
    }
}

/// Unary negation: integer negation that overflows becomes a real, and `NULL`
/// stays `NULL`. Text and blobs are converted numerically first, as in SQLite.
pub(crate) fn negate_cell(cell: &Cell) -> Cell {
    match numeric(cell) {
        None => Cell::Null,
        Some(Number::Int(value)) => match value.checked_neg() {
            Some(value) => Cell::Integer(value),
            None => Cell::Real(-(value as f64)),
        },
        Some(Number::Real(value)) => Cell::Real(-value),
    }
}

/// Arithmetic with SQLite rules: `NULL` propagates, integer operators truncate
/// and fall back to a real on overflow, and division by zero yields `NULL`.
pub(crate) fn arithmetic(op: BinaryOp, left: &Cell, right: &Cell) -> Cell {
    let (Some(left), Some(right)) = (numeric(left), numeric(right))
    else {
        return Cell::Null;
    };
    match op {
        BinaryOp::Add => match (left, right) {
            (Number::Int(left), Number::Int(right)) => match left.checked_add(right) {
                Some(value) => Cell::Integer(value),
                None => Cell::Real(left as f64 + right as f64),
            },
            (left, right) => Cell::Real(number_f64(left) + number_f64(right)),
        },
        BinaryOp::Subtract => match (left, right) {
            (Number::Int(left), Number::Int(right)) => match left.checked_sub(right) {
                Some(value) => Cell::Integer(value),
                None => Cell::Real(left as f64 - right as f64),
            },
            (left, right) => Cell::Real(number_f64(left) - number_f64(right)),
        },
        BinaryOp::Multiply => match (left, right) {
            (Number::Int(left), Number::Int(right)) => match left.checked_mul(right) {
                Some(value) => Cell::Integer(value),
                None => Cell::Real(left as f64 * right as f64),
            },
            (left, right) => Cell::Real(number_f64(left) * number_f64(right)),
        },
        BinaryOp::Divide => match (left, right) {
            // SQLite returns NULL rather than infinity for any zero divisor.
            (_, Number::Int(0)) | (_, Number::Real(0.0)) => Cell::Null,
            (Number::Int(left), Number::Int(right)) => match left.checked_div(right) {
                Some(value) => Cell::Integer(value),
                None => Cell::Real(left as f64 / right as f64),
            },
            (left, right) => Cell::Real(number_f64(left) / number_f64(right)),
        },
        // `%` converts both operands to 64-bit integers, keeps the dividend's
        // sign, returns a real when either operand was real, and yields NULL
        // when the truncated divisor is zero.
        BinaryOp::Modulo => {
            let real = matches!(&left, Number::Real(_)) || matches!(&right, Number::Real(_));
            let (left, right) = (number_i64(left), number_i64(right));
            if right == 0 {
                Cell::Null
            }
            else {
                // `i64::MIN % -1` overflows, and SQLite answers `0` for it.
                let remainder = left.checked_rem(right).unwrap_or(0);
                if real { Cell::Real(remainder as f64) } else { Cell::Integer(remainder) }
            }
        }
        // `||` never reaches arithmetic: it is evaluated by its own branch.
        BinaryOp::Concat => Cell::Null,
        // `->` / `->>` 同样由独立分支求值，这里只是穷尽匹配。
        BinaryOp::Arrow | BinaryOp::ArrowText => Cell::Null,
        BinaryOp::ShiftLeft => Cell::Integer(shift(number_i64(left), number_i64(right), true)),
        BinaryOp::ShiftRight => Cell::Integer(shift(number_i64(left), number_i64(right), false)),
        BinaryOp::BitAnd => Cell::Integer(number_i64(left) & number_i64(right)),
        BinaryOp::BitOr => Cell::Integer(number_i64(left) | number_i64(right)),
    }
}

/// `~`: one's complement of the 64-bit integer conversion, `NULL` stays `NULL`.
pub(crate) fn bit_not(cell: &Cell) -> Cell {
    match numeric(cell) {
        Some(number) => Cell::Integer(!number_i64(number)),
        None => Cell::Null,
    }
}

pub(crate) fn number_i64(number: Number) -> i64 {
    match number {
        Number::Int(value) => value,
        Number::Real(value) => value as i64,
    }
}

/// SQLite shifts 64-bit integers: a negative count reverses the direction,
/// counts of 64 or more saturate, and `>>` keeps the sign bit.
pub(crate) fn shift(value: i64, count: i64, left: bool) -> i64 {
    let magnitude = count.unsigned_abs();
    let leftward = left != (count < 0);
    if leftward {
        if magnitude >= 64 { 0 } else { ((value as u64) << magnitude) as i64 }
    }
    else if magnitude >= 64 {
        if value < 0 { -1 } else { 0 }
    }
    else {
        value >> magnitude
    }
}

/// `IN` with three-valued logic: a hit is true (false for `NOT IN`), a miss
/// with any `NULL` candidate is unknown, and a `NULL` probe is always unknown.
pub(crate) fn in_result(value: &Cell, list: &[Cell], negated: bool) -> Cell {
    if matches!(value, Cell::Null) {
        return Cell::Null;
    }
    let mut saw_null = false;
    for item in list {
        if matches!(item, Cell::Null) {
            saw_null = true;
        }
        else if compare_values(value, item, CompareOp::Eq, Collation::Binary) == Some(true) {
            return Cell::Integer(i64::from(!negated));
        }
    }
    if saw_null { Cell::Null } else { Cell::Integer(i64::from(negated)) }
}

/// Canonical key set for an `IN (SELECT ...)` value list, or `None` when an
/// item has no canonical signature, so membership has to scan the list.
pub(crate) fn in_list_keys(values: &[Cell]) -> Option<HashSet<ProbeKey>> {
    let mut keys = HashSet::with_capacity(values.len());
    for value in values {
        match probe_key(value, Collation::Binary) {
            Some(key) if !matches!(key, ProbeKey::Wide(_)) => {
                keys.insert(key);
            }
            _ => return None,
        }
    }
    Some(keys)
}

/// Membership with the same three-valued result as [`in_result`], using the
/// canonical key set when the whole list had one.
pub(crate) fn in_result_cached(value: &Cell, list: &[Cell], keys: Option<&HashSet<ProbeKey>>, saw_null: bool, negated: bool) -> Cell {
    if matches!(value, Cell::Null) {
        return Cell::Null;
    }
    if let Some(keys) = keys
        && let Some(key) = probe_key(value, Collation::Binary)
        && !matches!(key, ProbeKey::Wide(_))
    {
        return if keys.contains(&key) {
            Cell::Integer(i64::from(!negated))
        }
        else if saw_null {
            Cell::Null
        }
        else {
            Cell::Integer(i64::from(negated))
        };
    }
    in_result(value, list, negated)
}

/// Canonical hash signature of one output row, or `None` when a cell falls
/// outside the canonical form and dedup must fall back to the linear scan.
pub fn row_signature(row: &[Cell], collations: &[Collation]) -> Option<Vec<ProbeKey>> {
    row.iter()
        .enumerate()
        .map(|(position, cell)| match probe_key(cell, collations.get(position).copied().unwrap_or_default()) {
            Some(part) if !matches!(part, ProbeKey::Wide(_)) => Some(part),
            _ => None,
        })
        .collect()
}

/// Row equality under the engine's comparison rules and the given collations.
pub fn rows_equal_with(left: &[Cell], right: &[Cell], collations: &[Collation]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).enumerate().all(|(position, (first, second))| {
            compare_cells_with(first, second, collations.get(position).copied().unwrap_or_default()) == Ordering::Equal
        })
}
