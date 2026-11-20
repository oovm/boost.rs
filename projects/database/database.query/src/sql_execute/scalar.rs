//! 标量内建函数与时间/数值格式化。

use super::*;

/// 对一行求值标量内建函数。参数已按从左到右求值完毕。
///
/// `dialect` 参与的函数只有这些真正分叉的语义：`LENGTH` 的字节/字符计数、
/// `CONCAT` 的 NULL 传播、`SUBSTRING` 的越界裁剪与 `TRIM` 的显式字符集。
pub fn eval_scalar_function(function: SqlScalarFunction, args: &[SqlValue], dialect: SqlDialect) -> Result<SqlValue, SqlFrontendError> {
    use SqlValue::{Integer, Null, Number, String as Text};
    // 二进制实参只在语义明确的函数里有定义。文本谓词把 `CAST(... AS BINARY)` 的
    // 恒等字节视图按 UTF-8 解码（见 [`text_view`]），无法解码时报类型错误；
    // 其余函数没有无损的文本形式，显式报类型错误，而不是悄悄退化成 NULL。
    if !matches!(
        function,
        SqlScalarFunction::Length
            | SqlScalarFunction::Coalesce
            | SqlScalarFunction::Instr
            | SqlScalarFunction::Strpos
            | SqlScalarFunction::Left
            | SqlScalarFunction::Right
    ) && args.iter().any(|value| matches!(value, SqlValue::Bytes(_)))
    {
        return Err(SqlFrontendError::new(SqlErrorKind::TypeMismatch, format!("{}() is not defined for binary values", function.name())));
    }
    Ok(match function {
        SqlScalarFunction::Version => Text(crate::version_label()),
        // 未选择数据库 / schema 时，MySQL 与 PostgreSQL 都返回 NULL。
        SqlScalarFunction::Database | SqlScalarFunction::CurrentSchema => Null,
        SqlScalarFunction::Now => Text(current_timestamp()),
        SqlScalarFunction::CurrentDate => Text(current_date()),
        SqlScalarFunction::CurrentTime => Text(current_time()),
        SqlScalarFunction::Curdate | SqlScalarFunction::Curtime => {
            // `CURDATE()` / `CURTIME()` 是 MySQL 拼写，PostgreSQL 只认
            // `CURRENT_DATE` / `CURRENT_TIME`，报 `42883` 而不是语法错误。
            if dialect == SqlDialect::Postgres {
                return Err(SqlFrontendError::new(
                    SqlErrorKind::UnknownFunction,
                    format!(
                        "{}() is a MySQL spelling, PostgreSQL spells it CURRENT_{}",
                        function.name(),
                        if function == SqlScalarFunction::Curdate { "DATE" } else { "TIME" }
                    ),
                ));
            }
            Text(if function == SqlScalarFunction::Curdate { current_date() } else { current_time() })
        }
        SqlScalarFunction::Lower => match scalar_text(args, 0) {
            Some(text) => Text(text.to_lowercase()),
            None => Null,
        },
        SqlScalarFunction::Upper => match scalar_text(args, 0) {
            Some(text) => Text(text.to_uppercase()),
            None => Null,
        },
        // MySQL 的 `LENGTH` 是 UTF-8 字节数，PostgreSQL 的 `length` 是字符数。
        // 二进制值在两种方言下都按 octets 计数，结果一致。
        SqlScalarFunction::Length => match args.first() {
            Some(SqlValue::Bytes(bytes)) => Integer(i64::try_from(bytes.len()).unwrap_or(i64::MAX)),
            _ => match scalar_text(args, 0) {
                Some(text) => Integer(match dialect {
                    SqlDialect::Mysql => i64::try_from(text.len()).unwrap_or(i64::MAX),
                    SqlDialect::Postgres => i64::try_from(text.chars().count()).unwrap_or(i64::MAX),
                }),
                None => Null,
            },
        },
        SqlScalarFunction::CharLength => match scalar_text(args, 0) {
            Some(text) => Integer(i64::try_from(text.chars().count()).unwrap_or(i64::MAX)),
            None => Null,
        },
        SqlScalarFunction::Substring => {
            let (text, start) = match (scalar_text(args, 0), scalar_integer(args, 1)) {
                (Some(text), Some(start)) => (text, start),
                _ => return Ok(Null),
            };
            let length = match args.get(2) {
                None => None,
                Some(Null) => return Ok(Null),
                Some(_) => match scalar_integer(args, 2) {
                    Some(value) => Some(value),
                    None => return Ok(Null),
                },
            };
            substring_value(&text, start, length, dialect)?
        }
        SqlScalarFunction::Trim | SqlScalarFunction::Ltrim | SqlScalarFunction::Rtrim => {
            // 显式 trim 字符集是 PostgreSQL 的 `TRIM(x, chars)` 形式。MySQL 只有
            // `TRIM([BOTH|LEADING|TRAILING] chars FROM x)`，两参数调用在 MySQL 是语法错误。
            if dialect == SqlDialect::Mysql && args.len() > 1 {
                return Err(SqlFrontendError::syntax(format!(
                    "{}() with an explicit trim character set is PostgreSQL syntax",
                    function.name()
                )));
            }
            let text = match scalar_text(args, 0) {
                Some(text) => text,
                None => return Ok(Null),
            };
            let characters = match args.get(1) {
                None => None,
                Some(Null) => return Ok(Null),
                Some(_) => scalar_text(args, 1).map(|set| set.chars().collect::<Vec<_>>()),
            };
            // 默认只删除半角空格，两方言一致（MySQL 的 `TRIM(str)` 与 PostgreSQL 的 `trim(str)`）。
            let is_trimmed = |character: char| match characters.as_ref() {
                Some(set) => set.contains(&character),
                None => character == ' ',
            };
            Text(match function {
                SqlScalarFunction::Ltrim => text.trim_start_matches(is_trimmed).to_string(),
                SqlScalarFunction::Rtrim => text.trim_end_matches(is_trimmed).to_string(),
                _ => text.trim_matches(is_trimmed).to_string(),
            })
        }
        SqlScalarFunction::Replace => {
            let (text, from, to) = match (scalar_text(args, 0), scalar_text(args, 1), scalar_text(args, 2)) {
                (Some(text), Some(from), Some(to)) => (text, from, to),
                _ => return Ok(Null),
            };
            // 两方言都把空搜索串当作“无需替换”，而不是在每个字符之间插入替换串。
            Text(if from.is_empty() { text } else { text.replace(&from, &to) })
        }
        SqlScalarFunction::Concat => {
            let mut joined = String::new();
            for index in 0..args.len() {
                match scalar_text(args, index) {
                    Some(text) => joined.push_str(&text),
                    // MySQL：任一参数为 NULL，结果是 NULL。
                    None if dialect == SqlDialect::Mysql => return Ok(Null),
                    // PostgreSQL 的 `concat` 跳过 NULL 参数，把它当作空串。
                    None => {}
                }
            }
            Text(joined)
        }
        SqlScalarFunction::Coalesce => args.iter().find(|value| **value != Null).cloned().unwrap_or(Null),
        SqlScalarFunction::Instr | SqlScalarFunction::Strpos => {
            // 两种拼写语义相同（1-based 字符位置，未找到为 0），但只在各自方言里可调用。
            let allowed = match dialect {
                SqlDialect::Mysql => SqlScalarFunction::Instr,
                SqlDialect::Postgres => SqlScalarFunction::Strpos,
            };
            if function != allowed {
                let (owner, alternate) = if function == SqlScalarFunction::Instr {
                    ("MySQL", SqlScalarFunction::Strpos)
                }
                else {
                    ("PostgreSQL", SqlScalarFunction::Instr)
                };
                return Err(SqlFrontendError::new(
                    SqlErrorKind::UnknownFunction,
                    format!("{}() is a {owner} spelling, the other dialect spells it {}()", function.name(), alternate.name()),
                ));
            }
            let haystack = match text_view(args, 0)? {
                Some(text) => text,
                None => return Ok(Null),
            };
            let needle = match text_view(args, 1)? {
                Some(text) => text,
                None => return Ok(Null),
            };
            Integer(character_position(&haystack, &needle))
        }
        SqlScalarFunction::Left | SqlScalarFunction::Right => {
            let text = match text_view(args, 0)? {
                Some(text) => text,
                None => return Ok(Null),
            };
            let count = match scalar_integer(args, 1) {
                Some(count) => count,
                None => return Ok(Null),
            };
            Text(edge_characters(&text, count, function == SqlScalarFunction::Right))
        }
        SqlScalarFunction::Nextval | SqlScalarFunction::Currval | SqlScalarFunction::Setval => {
            // Sequence functions are the only volatile scalars: they read and
            // advance state the statement installed, and fail explicitly when no
            // catalog is present instead of inventing a value.
            let catalog = super::sequence_catalog().ok_or_else(|| {
                SqlFrontendError::new(
                    SqlErrorKind::Unsupported,
                    format!("{}() needs a sequence catalog, which this statement did not install", function.name()),
                )
            })?;
            let name = match args.first() {
                Some(Text(text)) => text.clone(),
                Some(Null) | None => return Ok(Null),
                Some(_) => {
                    return Err(SqlFrontendError::new(
                        SqlErrorKind::TypeMismatch,
                        format!("{}() takes the sequence name as a string", function.name()),
                    ));
                }
            };
            match function {
                SqlScalarFunction::Nextval => Integer(catalog.nextval(&name)?),
                SqlScalarFunction::Currval => Integer(catalog.currval(&name)?),
                _ => {
                    let Some(Integer(value)) = args.get(1)
                    else {
                        return Err(SqlFrontendError::new(SqlErrorKind::TypeMismatch, "SETVAL() takes an integer value"));
                    };
                    let is_called = match args.get(2) {
                        None => true,
                        Some(SqlValue::Boolean(flag)) => *flag,
                        Some(_) => {
                            return Err(SqlFrontendError::new(SqlErrorKind::TypeMismatch, "SETVAL() takes a boolean `is_called`"));
                        }
                    };
                    Integer(catalog.setval(&name, *value, is_called)?)
                }
            }
        }
        SqlScalarFunction::Abs => match args.first() {
            Some(Integer(value)) => Integer(value.checked_abs().ok_or_else(|| numeric_overflow_error("ABS"))?),
            Some(Number(value)) => Number(value.abs()),
            Some(Null) | None => Null,
            Some(_) => Number(0.0),
        },
        SqlScalarFunction::Round => {
            let digits = match args.get(1) {
                Some(Integer(value)) => Some(*value as f64),
                Some(Number(value)) => Some(*value),
                _ => None,
            };
            match args.first() {
                Some(Integer(value)) => match digits {
                    Some(digits) if digits < 0.0 => {
                        let factor = 10f64.powi(digits.abs() as i32);
                        Number((*value as f64 / factor).round() * factor)
                    }
                    _ => Integer(*value),
                },
                Some(Number(value)) => match digits {
                    Some(digits) => {
                        let factor = 10f64.powi(digits as i32);
                        Number((value * factor).round() / factor)
                    }
                    None => Number(value.round()),
                },
                Some(Null) | None => Null,
                Some(_) => Number(0.0),
            }
        }
        SqlScalarFunction::NullIf => {
            let (Some(a), Some(b)) = (args.first(), args.get(1))
            else {
                return Ok(Null);
            };
            if !matches!(a, Null) && !matches!(b, Null) && compare_values(Some(a), Some(b)) == std::cmp::Ordering::Equal {
                Null
            }
            else {
                a.clone()
            }
        }
        SqlScalarFunction::Greatest | SqlScalarFunction::Least => {
            // MySQL：任一实参为 NULL 结果即 NULL；PostgreSQL：忽略 NULL 实参，全 NULL 才返回 NULL。
            if dialect == SqlDialect::Mysql && args.iter().any(|value| matches!(value, Null)) {
                return Ok(Null);
            }
            let mut best: Option<&SqlValue> = None;
            for value in args.iter().filter(|value| !matches!(value, Null)) {
                best = Some(match best {
                    None => value,
                    Some(current) => {
                        let greater = compare_values(Some(value), Some(current)) == std::cmp::Ordering::Greater;
                        if matches!(function, SqlScalarFunction::Greatest) == greater { value } else { current }
                    }
                });
            }
            best.cloned().unwrap_or(Null)
        }
        SqlScalarFunction::DateTrunc => {
            // `DATE_TRUNC` 是 PostgreSQL 拼写，MySQL 没有等价函数。
            if dialect == SqlDialect::Mysql {
                return Err(SqlFrontendError::new(
                    SqlErrorKind::UnknownFunction,
                    "DATE_TRUNC() is a PostgreSQL function, MySQL has no equivalent",
                ));
            }
            let (Some(field), Some(source)) = (scalar_text(args, 0), scalar_text(args, 1))
            else {
                return Ok(Null);
            };
            date_trunc_value(&field, &source)?
        }
        SqlScalarFunction::Extract => {
            // `EXTRACT(field FROM source)` 在绑定层展开成 `extract(field, source)`：
            // 第一个实参是大写归一的字段名文本，NULL 源沿用上层的 NULL 传播。
            let (Some(field), Some(source)) = (scalar_text(args, 0), scalar_text(args, 1))
            else {
                return Ok(Null);
            };
            extract_value(&field, &source)?
        }
    })
}

/// `DATE_TRUNC(field, source)`：把规范 `YYYY-MM-DD [HH:MM:SS]` 文本按 `field` 粒度截断。
///
/// 网关的时间表示是 UTC 文本，截断是纯文本算术；小数秒与时区后缀在秒位之后被丢弃。
fn date_trunc_value(field: &str, source: &str) -> Result<SqlValue, SqlFrontendError> {
    let invalid = || SqlFrontendError::new(SqlErrorKind::TypeMismatch, format!("DATE_TRUNC source `{source}` is not a date or timestamp"));
    let Some(parts) = parse_datetime_parts(source) else {
        return Err(invalid());
    };
    let DateTimeParts { year, month, day, hour, minute, second } = parts;
    let (month, day) = (month as i64, day as i64);
    let date = format!("{year:04}-{month:02}-{day:02}");
    Ok(match field.to_ascii_lowercase().as_str() {
        "year" => SqlValue::String(format!("{year:04}-01-01 00:00:00")),
        "month" => SqlValue::String(format!("{year:04}-{month:02}-01 00:00:00")),
        "day" => SqlValue::String(format!("{date} 00:00:00")),
        "hour" => SqlValue::String(format!("{date} {hour:02}:00:00")),
        "minute" => SqlValue::String(format!("{date} {hour:02}:{minute:02}:00")),
        "second" => SqlValue::String(format!("{date} {hour:02}:{minute:02}:{second:02}")),
        other => {
            return Err(SqlFrontendError::new(
                SqlErrorKind::Unsupported,
                format!("DATE_TRUNC field `{other}` is outside the gateway subset, use YEAR, MONTH, DAY, HOUR, MINUTE or SECOND"),
            ));
        }
    })
}

/// 规范 `YYYY-MM-DD [HH:MM:SS]` 文本解析出的日期时间分量。
pub(crate) struct DateTimeParts {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// 把规范日期时间文本解析成 [`DateTimeParts`]；`None` 表示它不是日期或时间戳。
///
/// 小数秒与时区后缀在秒位之后被丢弃，与 `DATE_TRUNC` 的既有口径一致；`EXTRACT` 与
/// INTERVAL 加减共用这一份解析。
pub(crate) fn parse_datetime_parts(source: &str) -> Option<DateTimeParts> {
    let (date_part, time_part) = match source.split_once([' ', 'T']) {
        Some((date, time)) => (date, time),
        None => (source, ""),
    };
    let mut dates = date_part.split('-');
    let (Some(year), Some(month), Some(day)) = (dates.next(), dates.next(), dates.next()) else { return None };
    let (Ok(year), Ok(month), Ok(day)) = (year.parse::<u32>(), month.parse::<u32>(), day.parse::<u32>()) else { return None };
    let digits =
        |text: Option<&str>| -> u32 { text.unwrap_or("0").chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0) };
    let mut times = time_part.split(':');
    let (hour, minute, second) = (digits(times.next()), digits(times.next()), digits(times.next()));
    Some(DateTimeParts { year: year as i64, month, day, hour, minute, second })
}

/// `EXTRACT(field FROM source)`：从规范日期时间文本里取整数日期字段。
///
/// 网关与原生 VOS 引擎共用这一份实现，两个前端对同一个 `EXTRACT` 得到同一个整数。
pub fn extract_value(field: &str, source: &str) -> Result<SqlValue, SqlFrontendError> {
    let invalid = || SqlFrontendError::new(SqlErrorKind::TypeMismatch, format!("EXTRACT source `{source}` is not a date or timestamp"));
    let Some(parts) = parse_datetime_parts(source) else {
        return Err(invalid());
    };
    let days = days_from_civil(parts.year, parts.month, parts.day);
    let value = match field.to_ascii_uppercase().as_str() {
        "YEAR" => parts.year,
        "MONTH" => i64::from(parts.month),
        "DAY" => i64::from(parts.day),
        "HOUR" => i64::from(parts.hour),
        "MINUTE" => i64::from(parts.minute),
        "SECOND" => i64::from(parts.second),
        "QUARTER" => (i64::from(parts.month) + 2) / 3,
        "EPOCH" => days * 86_400 + i64::from(parts.hour) * 3_600 + i64::from(parts.minute) * 60 + i64::from(parts.second),
        // PostgreSQL 的 `DOW` 以周日为 0；1970-01-01（days=0）是周四。
        "DOW" => (days + 4).rem_euclid(7),
        "DOY" => days - days_from_civil(parts.year, 1, 1) + 1,
        // ISO 周号：周一为 1、周日为 7，`(ordinal - weekday + 10) / 7`。
        "WEEK" => {
            let ordinal = days - days_from_civil(parts.year, 1, 1) + 1;
            let weekday = (days + 3).rem_euclid(7) + 1;
            (ordinal - weekday + 10).div_euclid(7)
        }
        other => {
            return Err(SqlFrontendError::new(
                SqlErrorKind::Unsupported,
                format!("EXTRACT field `{other}` is outside the gateway subset, use YEAR, MONTH, DAY, HOUR, MINUTE, SECOND, EPOCH, DOW, DOY, WEEK or QUARTER"),
            ));
        }
    };
    Ok(SqlValue::Integer(value))
}

/// [`civil_from_days`] 的逆运算：公历日期距 1970-01-01 的天数。
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = (i64::from(month) + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// 解析后的 `INTERVAL '<数量> <单位>'`：月级、日级与秒级三段偏移。
pub struct IntervalOffset {
    pub months: i64,
    pub days: i64,
    pub seconds: i64,
}

/// 解析 INTERVAL 文本：`数量 单位` 成对出现，单位接受单复数拼写。
///
/// 网关与原生 VOS 引擎共用这一份解析，两个前端对同一段 INTERVAL 文本得到同一组偏移。
pub fn parse_interval(text: &str) -> Result<IntervalOffset, SqlFrontendError> {
    let malformed = || SqlFrontendError::new(SqlErrorKind::TypeMismatch, format!("`{text}` is not an INTERVAL of the form `<quantity> <unit>`"));
    let tokens = text.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() || !tokens.len().is_multiple_of(2) {
        return Err(malformed());
    }
    let (mut months, mut days, mut seconds) = (0f64, 0f64, 0f64);
    let mut index = 0;
    while index < tokens.len() {
        let quantity: f64 = tokens[index].parse().map_err(|_| malformed())?;
        let unit = tokens[index + 1].trim_end_matches(',').to_ascii_lowercase().trim_end_matches('s').to_string();
        match unit.as_str() {
            "year" => months += quantity * 12.0,
            "month" => months += quantity,
            "week" => days += quantity * 7.0,
            "day" => days += quantity,
            "hour" => seconds += quantity * 3_600.0,
            "minute" => seconds += quantity * 60.0,
            "second" => seconds += quantity,
            other => {
                return Err(SqlFrontendError::new(
                    SqlErrorKind::Unsupported,
                    format!("INTERVAL unit `{other}` is outside the gateway subset, use YEAR(S), MONTH(S), WEEK(S), DAY(S), HOUR(S), MINUTE(S) or SECOND(S)"),
                ));
            }
        }
        index += 2;
    }
    // 月级与日级偏移必须是整数；小数只允许出现在秒级，四舍五入到整秒。
    if months.fract() != 0.0 || days.fract() != 0.0 {
        return Err(malformed());
    }
    Ok(IntervalOffset { months: months as i64, days: days as i64, seconds: seconds.round() as i64 })
}

/// 日期时间文本与 INTERVAL 的加减，结果仍是规范日期时间文本。
///
/// 年/月按日历月平移（日裁剪到目标月最后一天），周/日与时间单位折算成绝对天与秒；
/// `sign` 在 `+` 时为 `1`、`-` 时为 `-1`。
pub fn apply_interval_to_datetime(base: &str, interval: &str, sign: i64) -> Result<SqlValue, SqlFrontendError> {
    let Some(parts) = parse_datetime_parts(base) else {
        return Err(SqlFrontendError::new(SqlErrorKind::TypeMismatch, format!("`{base}` is not a date or timestamp for INTERVAL arithmetic")));
    };
    let offset = parse_interval(interval)?;
    let total_months = parts.year * 12 + i64::from(parts.month) - 1 + sign * offset.months;
    let year = total_months.div_euclid(12);
    let month = (total_months.rem_euclid(12) + 1) as u32;
    let day = parts.day.min(days_in_month(year, month));
    let mut days = days_from_civil(year, month, day) + sign * offset.days;
    let mut seconds = i64::from(parts.hour) * 3_600 + i64::from(parts.minute) * 60 + i64::from(parts.second) + sign * offset.seconds;
    days += seconds.div_euclid(86_400);
    seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (seconds / 3_600, seconds % 3_600 / 60, seconds % 60);
    Ok(SqlValue::String(format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")))
}

/// 公历月的天数，闰年 2 月为 29 天。
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 { 29 } else { 28 },
    }
}

/// `SUBSTRING(x, start[, length])`，下标从 1 开始，字符（不是字节）计数。
///
/// 两方言共享 1-based 下标与“任一侧 NULL 即 NULL”，差异在于越界与负值：
///
/// - MySQL：`start = 0` 与负长度都返回空串，负起点从字符串结尾倒数后裁剪。
/// - PostgreSQL：`start < 1` 从第 1 个字符开始并把范围裁剪到 `start + length - 1`，
///   负长度报 `22011`，超出结尾返回空串。
pub(crate) fn substring_value(text: &str, start: i64, length: Option<i64>, dialect: SqlDialect) -> Result<SqlValue, SqlFrontendError> {
    let characters = text.chars().collect::<Vec<_>>();
    let total = i64::try_from(characters.len()).unwrap_or(i64::MAX);
    let (first, end) = match dialect {
        SqlDialect::Mysql => {
            if start == 0 || length.is_some_and(|length| length < 0) {
                return Ok(SqlValue::String(String::new()));
            }
            let offset = if start > 0 { start - 1 } else { total + start };
            let mut span = length.unwrap_or(total - offset);
            let mut begin = offset;
            if begin < 0 {
                span += begin;
                begin = 0;
            }
            (begin, begin + span)
        }
        SqlDialect::Postgres => {
            if length.is_some_and(|length| length < 0) {
                return Err(SqlFrontendError::new(SqlErrorKind::InvalidSubstringLength, "negative substring length not allowed"));
            }
            let begin = (start - 1).max(0);
            let end = match length {
                Some(length) => (start + length - 1).max(0),
                None => total,
            };
            (begin, end)
        }
    };
    let first = first.clamp(0, total) as usize;
    let end = end.clamp(0, total) as usize;
    Ok(SqlValue::String(if end <= first { String::new() } else { characters[first..end].iter().collect() }))
}

/// 把第 `index` 个参数取成字符串；NULL 或缺失返回 `None`。
pub fn scalar_text(args: &[SqlValue], index: usize) -> Option<String> {
    match args.get(index) {
        Some(SqlValue::String(text)) => Some(text.clone()),
        Some(SqlValue::Integer(value)) => Some(value.to_string()),
        Some(SqlValue::Number(value)) => Some(format_number(*value)),
        Some(SqlValue::Boolean(value)) => Some(if *value { "1" } else { "0" }.to_string()),
        // 二进制值是原始 octets，没有无损的文本形式。`LENGTH` 这类对二进制有
        // 明确定义的函数在上层单独判定，不经过本函数。
        Some(SqlValue::Bytes(_)) => None,
        Some(SqlValue::Array(_)) => None,
        Some(SqlValue::Null) | None => None,
    }
}

/// 把第 `index` 个参数取成整数；NULL 或无法解释成整数的文本返回 `None`。
pub(crate) fn scalar_integer(args: &[SqlValue], index: usize) -> Option<i64> {
    match args.get(index)? {
        SqlValue::Integer(value) => Some(*value),
        SqlValue::Number(value) => Some(*value as i64),
        SqlValue::String(text) => text.trim().parse::<i64>().ok(),
        SqlValue::Boolean(value) => Some(i64::from(*value)),
        SqlValue::Bytes(_) => None,
        SqlValue::Array(_) => None,
        SqlValue::Null => None,
    }
}

/// 把第 `index` 个参数取成文本视图，供文本谓词使用。
///
/// `CAST(x AS BINARY)` 的恒等字节视图只在能整体按 UTF-8 解码时才参与文本谓词；
/// 真正的二进制列无法解码，报类型不匹配而不是静默近似。NULL 与缺失返回 `None`。
pub(crate) fn text_view(args: &[SqlValue], index: usize) -> Result<Option<String>, SqlFrontendError> {
    match args.get(index) {
        None | Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Bytes(bytes)) => std::str::from_utf8(bytes).map(|text| Some(text.to_string())).map_err(|_| {
            SqlFrontendError::new(SqlErrorKind::TypeMismatch, "binary value is not valid UTF-8 text and cannot serve as text".to_string())
        }),
        _ => Ok(scalar_text(args, index)),
    }
}

/// `INSTR` / `STRPOS` 的 1-based 字符位置；未找到为 0，空 needle 为 1（两方言一致）。
pub(crate) fn character_position(haystack: &str, needle: &str) -> i64 {
    if needle.is_empty() {
        return 1;
    }
    match haystack.find(needle) {
        Some(byte_offset) => i64::try_from(haystack[..byte_offset].chars().count()).map_or(i64::MAX, |chars| chars + 1),
        None => 0,
    }
}

/// `LEFT` / `RIGHT` 按字符计数取一侧片段；非正数计数返回空串，超出长度取整串。
pub(crate) fn edge_characters(text: &str, count: i64, from_end: bool) -> String {
    if count <= 0 {
        return String::new();
    }
    let characters: Vec<char> = text.chars().collect();
    let take = usize::try_from(count).unwrap_or(usize::MAX).min(characters.len());
    if from_end { characters[characters.len() - take..].iter().collect() } else { characters[..take].iter().collect() }
}

/// 会话时间戳 `YYYY-MM-DD HH:MM:SS`，UTC。
pub(crate) fn current_timestamp() -> String {
    let seconds = current_utc_seconds();
    format!("{} {}", date_of(seconds), time_of(seconds))
}

/// 会话日期 `YYYY-MM-DD`，UTC。
pub(crate) fn current_date() -> String {
    date_of(current_utc_seconds())
}

/// 会话时间 `HH:MM:SS`，UTC。
pub(crate) fn current_time() -> String {
    time_of(current_utc_seconds())
}

/// 当前 UTC 秒数；时钟早于 Unix epoch 时按 0 处理。
pub(crate) fn current_utc_seconds() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.as_secs() as i64).unwrap_or(0)
}

pub(crate) fn date_of(seconds: i64) -> String {
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

pub(crate) fn time_of(seconds: i64) -> String {
    let remainder = seconds.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", remainder / 3600, (remainder % 3600) / 60, remainder % 60)
}

/// Gregorian date from days since 1970-01-01 (Howard Hinnant's civil_from_days).
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 { month_prime + 3 } else { month_prime - 9 };
    (if month <= 2 { year + 1 } else { year }, month as u32, day as u32)
}

/// Render a numeric SQL value with no trailing `.0` for integral values.
pub fn format_number(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 { format!("{}", value as i64) } else { format!("{value}") }
}
