//! Scalar function dispatch and the built-in function library of the SQLite
//! dialect evaluation mechanism.

use database_query::sqlite::{
    frontend::{CompareOp, ScalarFunction},
    value::Cell,
};
use database_storage::sqlite::Collation;

use super::{
    cells::*,
    datetime::{self, DateTimeFunction},
};
pub(crate) fn eval_function(name: ScalarFunction, args: &[Cell]) -> Cell {
    match name {
        // `abs()` 对整数保持整数，对实数与文本一律走二进制浮点，所以
        // `typeof(abs('-7'))` 是 `real` 而不是 `integer`。
        ScalarFunction::Abs => match &args[0] {
            Cell::Null => Cell::Null,
            Cell::Integer(value) => match value.checked_abs() {
                Some(value) => Cell::Integer(value),
                None => Cell::Real(-(*value as f64)),
            },
            cell => match numeric(cell) {
                Some(Number::Int(value)) => Cell::Real((value as f64).abs()),
                Some(Number::Real(value)) => Cell::Real(value.abs()),
                None => Cell::Null,
            },
        },
        ScalarFunction::Coalesce => args.iter().find(|cell| !matches!(cell, Cell::Null)).cloned().unwrap_or(Cell::Null),
        ScalarFunction::Length => match &args[0] {
            Cell::Null => Cell::Null,
            Cell::Blob(bytes) => Cell::Integer(bytes.len() as i64),
            Cell::Text(text) => Cell::Integer(text.chars().count() as i64),
            other => Cell::Integer(number_text(other).chars().count() as i64),
        },
        ScalarFunction::Typeof => Cell::Text(
            match &args[0] {
                Cell::Null => "null",
                Cell::Integer(_) => "integer",
                Cell::Real(_) => "real",
                Cell::Text(_) => "text",
                Cell::Blob(_) => "blob",
            }
            .to_string(),
        ),
        ScalarFunction::Lower => match &args[0] {
            Cell::Text(text) => Cell::Text(text.to_ascii_lowercase()),
            other => other.clone(),
        },
        ScalarFunction::Upper => match &args[0] {
            Cell::Text(text) => Cell::Text(text.to_ascii_uppercase()),
            other => other.clone(),
        },
        ScalarFunction::Substr => substr_cell(args),
        ScalarFunction::Round => round_cell(args),
        // `hex()` hashes the text form, so `hex(255)` is `323535` and a blob
        // keeps its bytes; `NULL` becomes the empty string.
        ScalarFunction::Hex => Cell::Text(match text_form(&args[0]) {
            Some(text) => text.bytes().map(|byte| format!("{byte:02X}")).collect(),
            None => String::new(),
        }),
        ScalarFunction::Quote => Cell::Text(quote_text(&args[0])),
        ScalarFunction::Random => Cell::Integer(next_random()),
        ScalarFunction::RandomBlob => Cell::Blob(random_blob(&args[0])),
        ScalarFunction::Instr => instr_cell(args),
        ScalarFunction::Trim => trim_cell(args, true, true),
        ScalarFunction::Ltrim => trim_cell(args, true, false),
        ScalarFunction::Rtrim => trim_cell(args, false, true),
        ScalarFunction::Replace => replace_cell(args),
        ScalarFunction::IfNull => match &args[0] {
            Cell::Null => args[1].clone(),
            value => value.clone(),
        },
        // `nullif(x, y)` 与 `x = y` 共用比较语义，比较结果未知时返回 `x`。
        ScalarFunction::NullIf => match compare_values(&args[0], &args[1], CompareOp::Eq, Collation::Binary) {
            Some(true) => Cell::Null,
            _ => args[0].clone(),
        },
        ScalarFunction::Printf => printf_cell(args),
        ScalarFunction::Char => Cell::Text(args.iter().map(|cell| character_for_code(integer_argument(cell))).collect()),
        ScalarFunction::Unicode => match &args[0] {
            Cell::Null => Cell::Null,
            cell => match cast_text(cell).chars().next() {
                Some(character) => Cell::Integer(character as i64),
                None => Cell::Null,
            },
        },
        ScalarFunction::Soundex => soundex_cell(&args[0]),
        ScalarFunction::Min => min_max_cell(args, false),
        ScalarFunction::Max => min_max_cell(args, true),
        ScalarFunction::Ceil => integral_or_real(args, f64::ceil),
        ScalarFunction::Floor => integral_or_real(args, f64::floor),
        ScalarFunction::Trunc => integral_or_real(args, f64::trunc),
        ScalarFunction::Sqrt => real_function(args, f64::sqrt),
        ScalarFunction::Pow => real_function2(args, f64::powf),
        ScalarFunction::Mod => real_function2(args, |left, right| left % right),
        ScalarFunction::Exp => real_function(args, f64::exp),
        ScalarFunction::Ln => positive_real_function(args, f64::ln),
        ScalarFunction::Log => log_cell(args),
        ScalarFunction::Log2 => positive_real_function(args, f64::log2),
        ScalarFunction::Log10 => positive_real_function(args, f64::log10),
        ScalarFunction::Sin => real_function(args, f64::sin),
        ScalarFunction::Cos => real_function(args, f64::cos),
        ScalarFunction::Tan => real_function(args, f64::tan),
        ScalarFunction::Asin => real_function(args, f64::asin),
        ScalarFunction::Acos => real_function(args, f64::acos),
        ScalarFunction::Atan => real_function(args, f64::atan),
        ScalarFunction::Atan2 => real_function2(args, f64::atan2),
        ScalarFunction::Sinh => real_function(args, f64::sinh),
        ScalarFunction::Cosh => real_function(args, f64::cosh),
        ScalarFunction::Tanh => real_function(args, f64::tanh),
        ScalarFunction::Asinh => real_function(args, f64::asinh),
        ScalarFunction::Acosh => real_function(args, f64::acosh),
        ScalarFunction::Atanh => real_function(args, f64::atanh),
        ScalarFunction::Degrees => real_function(args, f64::to_degrees),
        ScalarFunction::Radians => real_function(args, f64::to_radians),
        ScalarFunction::Pi => Cell::Real(std::f64::consts::PI),
        ScalarFunction::Sign => sign_cell(args),
        ScalarFunction::Date => datetime::eval(DateTimeFunction::Date, args),
        ScalarFunction::Time => datetime::eval(DateTimeFunction::Time, args),
        ScalarFunction::Datetime => datetime::eval(DateTimeFunction::Datetime, args),
        ScalarFunction::JulianDay => datetime::eval(DateTimeFunction::JulianDay, args),
        ScalarFunction::UnixEpoch => datetime::eval(DateTimeFunction::UnixEpoch, args),
        ScalarFunction::Strftime => datetime::eval(DateTimeFunction::Strftime, args),
        ScalarFunction::Timediff => datetime::eval(DateTimeFunction::Timediff, args),
        // JSON 函数在进入本函数前已交给 JSON 模块，这里只为穷尽匹配。
        ScalarFunction::JsonValid
        | ScalarFunction::JsonType
        | ScalarFunction::JsonArray
        | ScalarFunction::JsonObject
        | ScalarFunction::JsonQuote
        | ScalarFunction::JsonExtract
        | ScalarFunction::JsonArrayLength
        | ScalarFunction::JsonRemove
        | ScalarFunction::JsonSet
        | ScalarFunction::JsonInsert
        | ScalarFunction::JsonReplace
        | ScalarFunction::JsonPatch => Cell::Null,
    }
}

/// 多参数 `min` / `max`：任一参数为 `NULL` 时返回 `NULL`。`min` 在相等时
/// 取后一个参数，`max` 在相等时保留前一个参数，与 SQLite 的比较循环一致。
fn min_max_cell(args: &[Cell], want_max: bool) -> Cell {
    if args.iter().any(|cell| matches!(cell, Cell::Null)) {
        return Cell::Null;
    }
    let mut result = &args[0];
    for candidate in &args[1..] {
        let replace = if want_max {
            compare_values(result, candidate, CompareOp::Lt, Collation::Binary) == Some(true)
        }
        else {
            compare_values(result, candidate, CompareOp::GtEq, Collation::Binary) == Some(true)
        };
        if replace {
            result = candidate;
        }
    }
    result.clone()
}

fn character_for_code(code: i64) -> char {
    u32::try_from(code).ok().and_then(char::from_u32).unwrap_or('\u{FFFD}')
}

/// `soundex`：首字母大写后按 SQLite 的分组追加数字，最多四位，不足补 `0`。
fn soundex_cell(cell: &Cell) -> Cell {
    let text = cast_text(cell);
    let mut letters = text.chars().filter(|character| character.is_ascii_alphabetic());
    let Some(first) = letters.next()
    else {
        return Cell::Text("?000".to_string());
    };
    let mut encoded = String::new();
    encoded.push(first.to_ascii_uppercase());
    let mut previous = soundex_code(first);
    for letter in letters {
        let code = soundex_code(letter);
        if code == 0 {
            previous = 0;
            continue;
        }
        if code != previous {
            encoded.push((b'0' + code) as char);
            if encoded.len() == 4 {
                break;
            }
        }
        previous = code;
    }
    while encoded.len() < 4 {
        encoded.push('0');
    }
    Cell::Text(encoded)
}

fn soundex_code(character: char) -> u8 {
    match character.to_ascii_uppercase() {
        'B' | 'F' | 'P' | 'V' => 1,
        'C' | 'G' | 'J' | 'K' | 'Q' | 'S' | 'X' | 'Z' => 2,
        'D' | 'T' => 3,
        'L' => 4,
        'M' | 'N' => 5,
        'R' => 6,
        _ => 0,
    }
}

fn integer_argument(cell: &Cell) -> i64 {
    match cast_numeric(cell) {
        Cell::Integer(value) => value,
        Cell::Real(value) => value as i64,
        _ => 0,
    }
}

fn real_argument(cell: &Cell) -> f64 {
    match cast_numeric(cell) {
        Cell::Integer(value) => value as f64,
        Cell::Real(value) => value,
        _ => 0.0,
    }
}

/// `printf` / `format`：`NULL` 格式串返回 `NULL`，位置参数写法同样返回 `NULL`。
fn printf_cell(args: &[Cell]) -> Cell {
    let Some(format) = text_form(&args[0])
    else {
        return Cell::Null;
    };
    if format.contains('$') {
        return Cell::Null;
    }
    let characters: Vec<char> = format.chars().collect();
    let mut output = String::new();
    let mut index = 0usize;
    let mut argument = 1usize;
    while index < characters.len() {
        if characters[index] != '%' {
            output.push(characters[index]);
            index += 1;
            continue;
        }
        index += 1;
        if index >= characters.len() {
            output.push('%');
            break;
        }
        if characters[index] == '%' {
            output.push('%');
            index += 1;
            continue;
        }
        let mut flags = String::new();
        while index < characters.len() && "-+ 0#".contains(characters[index]) {
            flags.push(characters[index]);
            index += 1;
        }
        let mut width = 0usize;
        while index < characters.len() && characters[index].is_ascii_digit() {
            width = width.saturating_mul(10).saturating_add((characters[index] as u8 - b'0') as usize);
            index += 1;
        }
        let mut precision = None;
        if index < characters.len() && characters[index] == '.' {
            index += 1;
            let mut digits = 0usize;
            while index < characters.len() && characters[index].is_ascii_digit() {
                digits = digits.saturating_mul(10).saturating_add((characters[index] as u8 - b'0') as usize);
                index += 1;
            }
            precision = Some(digits);
        }
        while index < characters.len() && "lhL".contains(characters[index]) {
            index += 1;
        }
        if index >= characters.len() {
            output.push('%');
            break;
        }
        let conversion = characters[index];
        index += 1;
        // `%n` 不消费参数也不产出字符，解析继续。
        if conversion == 'n' {
            continue;
        }
        if !"diufFeEgGxXocszqQwp".contains(conversion) {
            // 未知转换终止解析：已有输出原样返回，否则整体为 NULL。
            return if output.is_empty() { Cell::Null } else { Cell::Text(output) };
        }
        let cell = args.get(argument).cloned().unwrap_or(Cell::Null);
        argument += 1;
        let rendered = render_conversion(conversion, &cell, &flags, width, precision);
        output.push_str(&rendered);
    }
    Cell::Text(output)
}

fn render_conversion(conversion: char, cell: &Cell, flags: &str, width: usize, precision: Option<usize>) -> String {
    let left = flags.contains('-');
    let zero = flags.contains('0') && !left;
    let plus = flags.contains('+');
    let space = flags.contains(' ');
    let alternate = flags.contains('#');
    match conversion {
        'd' | 'i' => {
            let value = integer_argument(cell);
            let digits = format!("{}", value.unsigned_abs());
            let digits = apply_integer_precision(digits, precision);
            let sign = if value < 0 {
                "-"
            }
            else if plus {
                "+"
            }
            else if space {
                " "
            }
            else {
                ""
            };
            pad_text(&format!("{sign}{digits}"), width, left, zero, sign.len())
        }
        'u' => {
            let value = integer_argument(cell) as u64;
            let digits = apply_integer_precision(value.to_string(), precision);
            pad_text(&digits, width, left, zero, 0)
        }
        'x' | 'X' => {
            let digits = format!("{:x}", integer_argument(cell) as u64);
            let digits = apply_integer_precision(digits, precision);
            let digits = if conversion == 'X' { digits.to_ascii_uppercase() } else { digits };
            let prefix = if alternate && integer_argument(cell) != 0 { if conversion == 'X' { "0X" } else { "0x" } } else { "" };
            pad_text(&format!("{prefix}{digits}"), width, left, zero, prefix.len())
        }
        'o' => {
            let digits = format!("{:o}", integer_argument(cell) as u64);
            let digits = apply_integer_precision(digits, precision);
            let prefix = if alternate && !digits.starts_with('0') { "0" } else { "" };
            pad_text(&format!("{prefix}{digits}"), width, left, zero, prefix.len())
        }
        'f' | 'F' => {
            let value = real_argument(cell);
            let keep = precision.unwrap_or(6);
            let (integer, fraction) = fixed_parts(value.abs(), keep);
            let mut body = integer;
            if keep > 0 {
                body.push('.');
                body.push_str(&fraction);
            }
            else if alternate {
                body.push('.');
            }
            let sign = if value.is_sign_negative() || value < 0.0 {
                "-"
            }
            else if plus {
                "+"
            }
            else if space {
                " "
            }
            else {
                ""
            };
            pad_text(&format!("{sign}{body}"), width, left, zero, sign.len())
        }
        'e' | 'E' => {
            let value = real_argument(cell);
            let keep = precision.unwrap_or(6);
            let (integer, fraction, exponent) = scientific_parts(value.abs(), keep);
            let letter = if conversion == 'E' { 'E' } else { 'e' };
            let sign = if value.is_sign_negative() || value < 0.0 {
                "-"
            }
            else if plus {
                "+"
            }
            else if space {
                " "
            }
            else {
                ""
            };
            let body = if fraction.is_empty() {
                format!("{integer}{letter}{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.unsigned_abs())
            }
            else {
                format!("{integer}.{fraction}{letter}{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.unsigned_abs())
            };
            pad_text(&format!("{sign}{body}"), width, left, zero, sign.len())
        }
        'g' | 'G' => {
            let value = real_argument(cell);
            let significant = precision.unwrap_or(6).max(1);
            let (integer, fraction, exponent) = scientific_parts(value.abs(), significant - 1);
            let mut body = if exponent < -4 || exponent >= significant as i32 {
                let fraction = trim_decimal_zeros(&fraction, alternate);
                let letter = if conversion == 'G' { 'E' } else { 'e' };
                let mantissa = if fraction.is_empty() { integer } else { format!("{integer}.{fraction}") };
                format!("{mantissa}{letter}{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.unsigned_abs())
            }
            else {
                let keep = (significant as i32 - 1 - exponent).max(0) as usize;
                let (integer, fraction) = fixed_parts(value.abs(), keep);
                let fraction = trim_decimal_zeros(&fraction, alternate);
                if fraction.is_empty() { if alternate { format!("{integer}.") } else { integer } } else { format!("{integer}.{fraction}") }
            };
            if conversion == 'G' {
                body = body.to_ascii_uppercase();
            }
            let sign = if value.is_sign_negative() || value < 0.0 {
                "-"
            }
            else if plus {
                "+"
            }
            else if space {
                " "
            }
            else {
                ""
            };
            pad_text(&format!("{sign}{body}"), width, left, zero, sign.len())
        }
        'c' => {
            let first: String = cast_text(cell).chars().next().map(|character| character.to_string()).unwrap_or_else(|| "\u{0000}".to_string());
            // 精度对 `%c` 表示重复次数，零与一都只输出一个字符。
            let body = first.repeat(precision.unwrap_or(1).max(1));
            // 字符串类转换忽略 `0` 标志，补位一律用空格。
            pad_text(&body, width, left, false, 0)
        }
        's' | 'z' => {
            let body = match (conversion, cell) {
                ('z', Cell::Null) => String::new(),
                _ => cast_text(cell),
            };
            let body = truncate_characters(body, precision);
            pad_text(&body, width, left, false, 0)
        }
        'q' | 'w' => {
            let body = match cell {
                Cell::Null => "(NULL)".to_string(),
                _ => {
                    let text = truncate_characters(cast_text(cell), precision);
                    if conversion == 'q' { text.replace('\'', "''") } else { text }
                }
            };
            pad_text(&body, width, left, false, 0)
        }
        'Q' => {
            let body = match cell {
                Cell::Null => "NULL".to_string(),
                _ => {
                    let text = truncate_characters(cast_text(cell), precision);
                    format!("'{}'", text.replace('\'', "''"))
                }
            };
            pad_text(&body, width, left, false, 0)
        }
        'p' => {
            let digits = format!("{:X}", integer_argument(cell) as u64);
            let digits = apply_integer_precision(digits, precision);
            let prefix = if alternate && integer_argument(cell) != 0 { "0x" } else { "" };
            pad_text(&format!("{prefix}{digits}"), width, left, zero, prefix.len())
        }
        // SQLite 对未知转换按文本渲染，这里保持同样的宽松行为。
        _ => pad_text(&cast_text(cell), width, left, zero, 0),
    }
}

fn apply_integer_precision(digits: String, precision: Option<usize>) -> String {
    match precision {
        Some(minimum) if digits.len() < minimum => format!("{}{digits}", "0".repeat(minimum - digits.len())),
        _ => digits,
    }
}

fn truncate_characters(text: String, precision: Option<usize>) -> String {
    match precision {
        Some(limit) => text.chars().take(limit).collect(),
        None => text,
    }
}

fn pad_text(body: &str, width: usize, left: bool, zero: bool, prefix_len: usize) -> String {
    if left {
        let count = body.chars().count();
        if count >= width {
            return body.to_string();
        }
        return format!("{body}{}", " ".repeat(width - count));
    }
    if zero {
        let prefix: String = body.chars().take(prefix_len).collect();
        let rest: String = body.chars().skip(prefix_len).collect();
        // 符号计入宽度，`0x` / 八进制前导 `0` 一类前缀不计入宽度。
        let sign_like = prefix.starts_with('-') || prefix.starts_with('+') || prefix.starts_with(' ');
        let measured = if sign_like { body.chars().count() } else { rest.chars().count() };
        if measured >= width {
            return body.to_string();
        }
        return format!("{prefix}{}{rest}", "0".repeat(width - measured));
    }
    let count = body.chars().count();
    if count >= width {
        return body.to_string();
    }
    format!("{}{body}", " ".repeat(width - count))
}

fn trim_decimal_zeros(fraction: &str, keep_zero: bool) -> String {
    if keep_zero {
        return fraction.to_string();
    }
    fraction.trim_end_matches('0').to_string()
}

/// 把 `value` 按十进制四舍五入到 `keep` 位小数，返回整数段与小数段。
fn fixed_parts(value: f64, keep: usize) -> (String, String) {
    let probe = format!("{value:.precision$}", precision = keep + 20);
    let (probe_integer, probe_fraction) = probe.split_once('.').unwrap_or((probe.as_str(), ""));
    let tail: String = probe_fraction.chars().skip(keep).collect();
    if tail.starts_with('5') && tail.chars().skip(1).all(|character| character == '0') {
        let mut digits: String = probe_integer.chars().chain(probe_fraction.chars().take(keep)).collect();
        digits = increment_decimal(&digits);
        let split = digits.len() - keep;
        return (digits[..split].to_string(), digits[split..].to_string());
    }
    let rounded = format!("{value:.precision$}", precision = keep);
    match rounded.split_once('.') {
        Some((integer, fraction)) => (integer.to_string(), fraction.to_string()),
        None => (rounded, String::new()),
    }
}

/// 把 `value` 规格化为 `整数位.小数位 e 指数`，尾数按十进制四舍五入。
fn scientific_parts(value: f64, keep: usize) -> (String, String, i32) {
    if value == 0.0 {
        return ("0".to_string(), "0".repeat(keep), 0);
    }
    let probe = format!("{value:.precision$e}", precision = keep + 20);
    let (probe_mantissa, probe_exponent) = probe.split_once('e').unwrap_or((probe.as_str(), "0"));
    let probe_digits: String = probe_mantissa.chars().filter(|character| character.is_ascii_digit()).collect();
    let tail: String = probe_digits.chars().skip(keep + 1).collect();
    let exponent: i32 = probe_exponent.parse().unwrap_or(0);
    let digits = if tail.starts_with('5') && tail.chars().skip(1).all(|character| character == '0') {
        increment_decimal(&probe_digits.chars().take(keep + 1).collect::<String>())
    }
    else {
        let rounded = format!("{value:.precision$e}", precision = keep);
        rounded
            .split_once('e')
            .map(|(mantissa, _)| mantissa.chars().filter(|character| character.is_ascii_digit()).collect())
            .unwrap_or_else(|| "0".to_string())
    };
    if digits.len() > keep + 1 {
        let digit = digits.chars().take(keep + 1).collect::<String>();
        return (digit[..1].to_string(), digit[1..].to_string(), exponent + 1);
    }
    (digits[..1].to_string(), digits[1..].to_string(), exponent)
}

/// `trim` 家族：按字符集去除两端字符，第二个参数缺省时只去除空格。
fn trim_cell(args: &[Cell], left: bool, right: bool) -> Cell {
    let Some(text) = text_form(&args[0])
    else {
        return Cell::Null;
    };
    let set = match args.get(1) {
        Some(cell) => match text_form(cell) {
            Some(set) => set,
            None => return Cell::Null,
        },
        None => " ".to_string(),
    };
    if set.is_empty() {
        return Cell::Text(text);
    }
    let characters: Vec<char> = text.chars().collect();
    let mut start = 0;
    let mut end = characters.len();
    if left {
        while start < end && set.chars().any(|candidate| candidate == characters[start]) {
            start += 1;
        }
    }
    if right {
        while end > start && set.chars().any(|candidate| candidate == characters[end - 1]) {
            end -= 1;
        }
    }
    Cell::Text(characters[start..end].iter().collect())
}

/// `replace(x, y, z)`：空 `y` 原样返回 `x`，任一参数为 `NULL` 时返回 `NULL`。
fn replace_cell(args: &[Cell]) -> Cell {
    let (Some(text), Some(pattern), Some(replacement)) = (text_form(&args[0]), text_form(&args[1]), text_form(&args[2]))
    else {
        return Cell::Null;
    };
    if pattern.is_empty() {
        return Cell::Text(text);
    }
    Cell::Text(text.replace(&pattern, &replacement))
}

/// `instr(x, y)`: the one-based character position of the first `y` inside
/// `x`, or zero when `y` does not occur. Either `NULL` propagates.
fn instr_cell(args: &[Cell]) -> Cell {
    match (text_form(&args[0]), text_form(&args[1])) {
        (Some(haystack), Some(needle)) => match haystack.find(&needle) {
            Some(byte) => Cell::Integer(haystack[..byte].chars().count() as i64 + 1),
            None => Cell::Integer(0),
        },
        _ => Cell::Null,
    }
}

/// `quote()` renders one SQL literal: text is single-quoted with doubled
/// quotes, blobs become `X'..'`, `NULL` becomes the word `NULL`, and reals use
/// the 15-digit form when it round-trips.
fn quote_text(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(value) => value.to_string(),
        Cell::Real(value) => quote_real(*value),
        Cell::Text(text) => format!("'{}'", text.replace('\'', "''")),
        Cell::Blob(bytes) => {
            let digits: String = bytes.iter().map(|byte| format!("{byte:02X}")).collect();
            format!("X'{digits}'")
        }
    }
}

fn quote_real(value: f64) -> String {
    let text = sqlite_real_text(value);
    if text.parse::<f64>() == Ok(value) {
        return text;
    }
    // SQLite falls back to 18 fractional digits when the short form loses
    // precision, with a sign and at least two exponent digits.
    let formatted = format!("{value:.18e}");
    match formatted.split_once('e') {
        Some((mantissa, exponent)) => {
            let exponent: i32 = exponent.parse().unwrap_or(0);
            format!("{mantissa}e{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.abs())
        }
        None => formatted,
    }
}

/// `random()` returns any 64-bit integer, so a counter-driven xorshift is
/// enough: tests only check the type and range.
fn next_random() -> i64 {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut state = STATE.load(Ordering::Relaxed);
    if state == 0 {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos() as u64);
        state = nanos ^ 0x9e37_79b9_7f4a_7c15;
    }
    // xorshift64*: cheap, deterministic and good enough for a random value.
    let mut next = state ^ (state >> 12);
    next ^= next << 25;
    next ^= next >> 27;
    STATE.store(next, Ordering::Relaxed);
    (next.wrapping_mul(0x2545_f491_4f6c_dd1d)) as i64
}

/// `randomblob(n)` yields `n` random bytes; `n` below one means a single byte,
/// which is also what a `NULL` argument produces.
fn random_blob(size: &Cell) -> Vec<u8> {
    let count = match numeric(size) {
        Some(number) => number_i64(number).max(1) as usize,
        None => 1,
    };
    let mut bytes = Vec::with_capacity(count);
    while bytes.len() < count {
        let mut value = next_random();
        for _ in 0..8 {
            if bytes.len() == count {
                break;
            }
            bytes.push(value as u8);
            value >>= 8;
        }
    }
    bytes
}

/// Text form used by `length()` for numeric input.
fn number_text(cell: &Cell) -> String {
    match cell {
        Cell::Integer(value) => value.to_string(),
        Cell::Real(value) => {
            if value.is_finite() && *value == value.trunc() && value.abs() < 1e15 {
                format!("{value:.1}")
            }
            else {
                value.to_string()
            }
        }
        Cell::Text(text) => text.clone(),
        Cell::Blob(_) => String::new(),
        Cell::Null => String::new(),
    }
}

fn substr_cell(args: &[Cell]) -> Cell {
    let start = match numeric(&args[1]) {
        Some(number) => match number {
            Number::Int(value) => value,
            Number::Real(value) => value as i64,
        },
        None => return Cell::Null,
    };
    let count = match args.get(2) {
        None => None,
        Some(cell) => match numeric(cell) {
            Some(Number::Int(value)) => Some(value),
            Some(Number::Real(value)) => Some(value as i64),
            None => return Cell::Null,
        },
    };
    match &args[0] {
        Cell::Null => Cell::Null,
        Cell::Blob(bytes) => {
            let (begin, end) = substr_bounds(bytes.len(), start, count);
            Cell::Blob(bytes[begin..end].to_vec())
        }
        _ => {
            let text = match &args[0] {
                Cell::Text(text) => text.clone(),
                other => number_text(other),
            };
            let chars = text.chars().collect::<Vec<_>>();
            let (begin, end) = substr_bounds(chars.len(), start, count);
            Cell::Text(chars[begin..end].iter().collect())
        }
    }
}

/// SQLite `substr` bounds: positive start is 1-based, non-positive start counts
/// from the right, and a negative length runs backwards from the start point.
fn substr_bounds(length: usize, start: i64, count: Option<i64>) -> (usize, usize) {
    let length = length as i64;
    let begin = if start > 0 {
        start - 1
    }
    else if start == 0 {
        -1
    }
    else {
        length + start
    };
    let (begin, end) = match count {
        None => (begin, length),
        Some(count) if count >= 0 => (begin, begin + count),
        Some(count) => (begin + count, begin),
    };
    (begin.clamp(0, length) as usize, end.clamp(0, length) as usize)
}

fn round_cell(args: &[Cell]) -> Cell {
    let value = match numeric(&args[0]) {
        Some(Number::Int(value)) => value as f64,
        Some(Number::Real(value)) => value,
        None => return Cell::Null,
    };
    let digits = match args.get(1) {
        None => 0,
        Some(cell) => match numeric(cell) {
            Some(Number::Int(value)) => value,
            Some(Number::Real(value)) => value.trunc() as i64,
            None => return Cell::Null,
        },
    };
    Cell::Real(round_digits(value, digits))
}

/// SQLite's `round()`: the digit count is clamped to `0..=30`, a zero count
/// truncates `value ± 0.5` toward zero, and every other count re-reads the
/// fixed-point text its `printf` produces, which rounds an exact decimal tie
/// away from zero.
fn round_digits(value: f64, digits: i64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let digits = digits.clamp(0, 30) as usize;
    if digits == 0 {
        return if value < 0.0 { (value - 0.5).trunc() } else { (value + 0.5).trunc() };
    }
    let magnitude = value.abs();
    let short = format!("{magnitude:.digits$}");
    // A tie is a `5` directly after the kept digits with nothing but zeros
    // behind it; Rust's own formatting rounds that tie to even, SQLite rounds
    // it away from zero.
    let wide = format!("{:.*}", digits + 25, magnitude);
    let fraction = wide.split_once('.').map(|(_, fraction)| fraction).unwrap_or("");
    let tie = fraction.len() > digits && fraction.as_bytes()[digits] == b'5' && fraction[digits + 1..].bytes().all(|byte| byte == b'0');
    let text = if tie { increment_decimal(&short) } else { short };
    let rounded: f64 = text.parse().unwrap_or(magnitude);
    if value < 0.0 { -rounded } else { rounded }
}

/// 数学函数的取数口径：整数与实数原样，文本必须整体可解析为数值，`NULL`
/// 与 blob 视为无值。SQLite 对文本套用 numeric affinity，因此 `'2abc'` 是
/// `NULL` 而不是前缀 `2`。
fn math_number(cell: &Cell) -> Option<Number> {
    match cell {
        Cell::Null => None,
        Cell::Integer(value) => Some(Number::Int(*value)),
        Cell::Real(value) => Some(Number::Real(*value)),
        Cell::Text(text) => parse_full_number(text),
        Cell::Blob(_) => None,
    }
}

/// 整体解析一段文本：两端空白可有，中途出现任何非数值字符即拒绝。整数
/// 字面量返回 [`Number::Int`]，带小数点或指数的返回 [`Number::Real`]。
fn parse_full_number(text: &str) -> Option<Number> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = trimmed.parse::<i64>() {
        return Some(Number::Int(value));
    }
    match trimmed.parse::<f64>() {
        Ok(value) if value.is_finite() => Some(Number::Real(value)),
        _ => None,
    }
}

/// 数学结果统一出口：`NaN` 记为 `NULL`，正负无穷保留为实数，与 SQLite 的
/// `sqlite3_result_double` 一致。
fn real_result(value: f64) -> Cell {
    if value.is_nan() { Cell::Null } else { Cell::Real(value) }
}

/// 一元数学函数：入参按 [`math_number`] 取数，输出恒为实数。
fn real_function(args: &[Cell], function: fn(f64) -> f64) -> Cell {
    match args.first().and_then(math_number) {
        Some(Number::Int(value)) => real_result(function(value as f64)),
        Some(Number::Real(value)) => real_result(function(value)),
        None => Cell::Null,
    }
}

/// 二元数学函数：两个入参都按 [`math_number`] 取数，输出恒为实数。
fn real_function2(args: &[Cell], function: fn(f64, f64) -> f64) -> Cell {
    let left = match args.first().and_then(math_number) {
        Some(Number::Int(value)) => value as f64,
        Some(Number::Real(value)) => value,
        None => return Cell::Null,
    };
    let right = match args.get(1).and_then(math_number) {
        Some(Number::Int(value)) => value as f64,
        Some(Number::Real(value)) => value,
        None => return Cell::Null,
    };
    real_result(function(left, right))
}

/// 定义域为正数的数学函数（`ln`、`log2`、`log10`）：非正入参返回 `NULL`，
/// 不把 `-inf` 泄漏成实数。
fn positive_real_function(args: &[Cell], function: fn(f64) -> f64) -> Cell {
    let value = match args.first().and_then(math_number) {
        Some(Number::Int(value)) => value as f64,
        Some(Number::Real(value)) => value,
        None => return Cell::Null,
    };
    if value <= 0.0 {
        return Cell::Null;
    }
    real_result(function(value))
}

/// `ceil` / `floor` / `trunc`：整数入参原样返回整数，实数入参返回取整后的
/// 实数，非数值文本与 `NULL` 返回 `NULL`。
fn integral_or_real(args: &[Cell], function: fn(f64) -> f64) -> Cell {
    match args.first().and_then(math_number) {
        Some(Number::Int(value)) => Cell::Integer(value),
        Some(Number::Real(value)) => real_result(function(value)),
        None => Cell::Null,
    }
}

/// `sign()`：返回 `-1`、`0` 或 `1`，恒为整数；入参不是数值时返回 `NULL`。
fn sign_cell(args: &[Cell]) -> Cell {
    match args.first().and_then(math_number) {
        Some(Number::Int(value)) => Cell::Integer(value.signum()),
        Some(Number::Real(value)) => {
            let sign = if value > 0.0 {
                1
            }
            else if value < 0.0 {
                -1
            }
            else {
                0
            };
            Cell::Integer(sign)
        }
        None => Cell::Null,
    }
}

/// `log()`：单参是十进制对数，双参时底数必须大于 1 且真数必须为正，否则
/// 返回 `NULL`（底数为 1 时 SQLite 同样返回 `NULL` 而不是无穷）。
fn log_cell(args: &[Cell]) -> Cell {
    if args.len() == 1 {
        let value = match args.first().and_then(math_number) {
            Some(Number::Int(value)) => value as f64,
            Some(Number::Real(value)) => value,
            None => return Cell::Null,
        };
        if value <= 0.0 {
            return Cell::Null;
        }
        return real_result(value.log10());
    }
    let base = match args.first().and_then(math_number) {
        Some(Number::Int(value)) => value as f64,
        Some(Number::Real(value)) => value,
        None => return Cell::Null,
    };
    let value = match args.get(1).and_then(math_number) {
        Some(Number::Int(value)) => value as f64,
        Some(Number::Real(value)) => value,
        None => return Cell::Null,
    };
    if base <= 1.0 || value <= 0.0 {
        return Cell::Null;
    }
    real_result(value.ln() / base.ln())
}

/// Increments the last decimal digit of a non-negative fixed-point string,
/// carrying through `9`s and into the integer part.
fn increment_decimal(text: &str) -> String {
    let mut characters: Vec<char> = text.chars().collect();
    let mut index = characters.len();
    loop {
        if index == 0 {
            characters.insert(0, '1');
            break;
        }
        index -= 1;
        match characters[index] {
            '.' => continue,
            '9' => characters[index] = '0',
            digit => {
                characters[index] = char::from(digit as u8 + 1);
                break;
            }
        }
    }
    characters.into_iter().collect()
}
