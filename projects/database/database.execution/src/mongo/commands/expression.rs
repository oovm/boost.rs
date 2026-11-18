//! 聚合表达式求值：算术 / 字符串 / 数组 / 日期 / 类型转换算子。

use super::*;

pub(crate) fn empty_variables() -> ExpressionVariables {
    BTreeMap::new()
}

/// A field-path expression evaluates to `None` when the field is missing, so
/// `$push` / `$addToSet` can skip it instead of turning it into `null`.
pub(crate) fn evaluate_expression_optional(document: &BsonDocument, expression: &BsonValue) -> Result<Option<BsonValue>, CommandError> {
    evaluate_expression_optional_vars(document, expression, &empty_variables())
}

pub(crate) fn evaluate_expression(document: &BsonDocument, expression: &BsonValue) -> Result<BsonValue, CommandError> {
    evaluate_expression_vars(document, expression, &empty_variables())
}

pub(crate) fn evaluate_expression_optional_vars(
    document: &BsonDocument,
    expression: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<Option<BsonValue>, CommandError> {
    match expression {
        BsonValue::String(variable) if variable.starts_with("$$") => Ok(variables.get(&variable[2..]).cloned()),
        BsonValue::String(path) if path.starts_with('$') => Ok(lookup_field(document, &path[1..]).cloned()),
        other => evaluate_expression_vars(document, other, variables).map(Some),
    }
}

pub(crate) fn evaluate_expression_vars(
    document: &BsonDocument,
    expression: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<BsonValue, CommandError> {
    match expression {
        BsonValue::String(path) if path.starts_with("$$") => Ok(variables.get(&path[2..]).cloned().unwrap_or(BsonValue::Null)),
        BsonValue::String(path) if path.starts_with('$') => Ok(lookup_field(document, &path[1..]).cloned().unwrap_or(BsonValue::Null)),
        BsonValue::Document(operator_document) if operator_document.fields.len() == 1 && operator_document.fields[0].0.starts_with('$') => {
            let (operator, argument) = &operator_document.fields[0];
            match operator.as_str() {
                "$literal" => Ok(argument.clone()),
                "$eq" | "$ne" | "$gt" | "$gte" | "$lt" | "$lte" => {
                    let BsonValue::Array(arguments) = argument
                    else {
                        return Err(CommandError::new(2, format!("{operator} requires an array of two expressions")));
                    };
                    if arguments.len() != 2 {
                        return Err(CommandError::new(2, format!("{operator} requires exactly two expressions")));
                    }
                    let left = evaluate_expression_vars(document, &arguments[0], variables)?;
                    let right = evaluate_expression_vars(document, &arguments[1], variables)?;
                    let ordering = compare_values(&left, &right);
                    Ok(BsonValue::Bool(match operator.as_str() {
                        "$eq" => ordering == Ordering::Equal,
                        "$ne" => ordering != Ordering::Equal,
                        "$gt" => ordering == Ordering::Greater,
                        "$gte" => ordering != Ordering::Less,
                        "$lt" => ordering == Ordering::Less,
                        _ => ordering != Ordering::Greater,
                    }))
                }
                "$in" => {
                    let BsonValue::Array(arguments) = argument
                    else {
                        return Err(CommandError::new(2, "$in requires an array of two expressions"));
                    };
                    if arguments.len() != 2 {
                        return Err(CommandError::new(2, "$in requires exactly two expressions"));
                    }
                    let needle = evaluate_expression_vars(document, &arguments[0], variables)?;
                    let haystack = evaluate_expression_vars(document, &arguments[1], variables)?;
                    let BsonValue::Array(values) = haystack
                    else {
                        return Err(CommandError::new(2, "$in second argument must be an array"));
                    };
                    Ok(BsonValue::Bool(values.iter().any(|value| compare_values(&needle, value) == Ordering::Equal)))
                }
                "$and" | "$or" => {
                    let BsonValue::Array(arguments) = argument
                    else {
                        return Err(CommandError::new(2, format!("{operator} requires an array of expressions")));
                    };
                    let mut result = operator == "$and";
                    for expression in arguments {
                        let value = evaluate_expression_vars(document, expression, variables)?;
                        let truthy = expression_truthy(&value);
                        if operator == "$and" {
                            result = result && truthy;
                        }
                        else {
                            result = result || truthy;
                        }
                    }
                    Ok(BsonValue::Bool(result))
                }
                "$not" => {
                    let BsonValue::Array(arguments) = argument
                    else {
                        return Err(CommandError::new(2, "$not requires an array of one expression"));
                    };
                    let first = arguments.first().ok_or_else(|| CommandError::new(2, "$not requires one expression"))?;
                    let value = evaluate_expression_vars(document, first, variables)?;
                    Ok(BsonValue::Bool(!expression_truthy(&value)))
                }
                "$cond" => {
                    let (condition, then_value, else_value) = match argument {
                        BsonValue::Array(arguments) if arguments.len() == 3 => (&arguments[0], &arguments[1], &arguments[2]),
                        BsonValue::Document(options) => {
                            let condition = options.get("if").ok_or_else(|| CommandError::new(17072, "$cond requires an 'if' argument"))?;
                            let then_value = options.get("then").ok_or_else(|| CommandError::new(17072, "$cond requires a 'then' argument"))?;
                            let else_value =
                                options.get("else").ok_or_else(|| CommandError::new(17072, "$cond requires an 'else' argument"))?;
                            (condition, then_value, else_value)
                        }
                        _ => {
                            return Err(CommandError::new(
                                17072,
                                "$cond requires either an array of three expressions or an object with 'if', 'then' and 'else'",
                            ));
                        }
                    };
                    let condition = evaluate_expression_vars(document, condition, variables)?;
                    if expression_truthy(&condition) {
                        evaluate_expression_vars(document, then_value, variables)
                    }
                    else {
                        evaluate_expression_vars(document, else_value, variables)
                    }
                }
                "$add" | "$subtract" | "$multiply" | "$divide" | "$mod" | "$pow" => {
                    evaluate_arithmetic(document, operator, argument, variables)
                }
                "$abs" | "$ceil" | "$floor" | "$sqrt" => {
                    let operand = evaluate_expression_vars(document, argument, variables)?;
                    match numeric_operand(&operand, operator)? {
                        ArithmeticOperand::Null => Ok(BsonValue::Null),
                        ArithmeticOperand::Integer { value, long } => match operator.as_str() {
                            "$abs" => integer_arithmetic_result(value.abs(), long, operator),
                            "$ceil" | "$floor" => integer_arithmetic_result(value, long, operator),
                            _ => {
                                if value < 0 {
                                    return Err(CommandError::new(28714, "$sqrt's argument must be greater than or equal to 0"));
                                }
                                Ok(BsonValue::Double((value as f64).sqrt()))
                            }
                        },
                        ArithmeticOperand::Double(number) => match operator.as_str() {
                            "$abs" => Ok(BsonValue::Double(number.abs())),
                            "$ceil" => Ok(BsonValue::Double(number.ceil())),
                            "$floor" => Ok(BsonValue::Double(number.floor())),
                            _ => {
                                if number < 0.0 {
                                    return Err(CommandError::new(28714, "$sqrt's argument must be greater than or equal to 0"));
                                }
                                Ok(BsonValue::Double(number.sqrt()))
                            }
                        },
                        ArithmeticOperand::DateTime(_) => unreachable!("numeric_operand rejects dates"),
                    }
                }
                "$round" | "$trunc" => {
                    let arguments = match argument {
                        BsonValue::Array(arguments) if !arguments.is_empty() && arguments.len() <= 2 => arguments.as_slice(),
                        _ => {
                            return Err(CommandError::new(2, format!("{operator} requires an array of one or two expressions")));
                        }
                    };
                    let number = evaluate_expression_vars(document, &arguments[0], variables)?;
                    let place = match arguments.get(1) {
                        Some(expression) => evaluate_expression_vars(document, expression, variables)?,
                        None => BsonValue::Int32(0),
                    };
                    let place = match integral_i64(&place) {
                        Some(place) if (-20..=100).contains(&place) => place as i32,
                        Some(_) => {
                            return Err(CommandError::new(2, format!("{operator} places must be between -20 and 100")));
                        }
                        None => {
                            return Err(CommandError::new(2, format!("{operator} places must be an integer")));
                        }
                    };
                    match numeric_operand(&number, operator)? {
                        ArithmeticOperand::Null => Ok(BsonValue::Null),
                        ArithmeticOperand::Integer { value, long } => {
                            let rounded = round_integer_to_place(value, place, operator == "$round");
                            integer_arithmetic_result(rounded, long, operator)
                        }
                        ArithmeticOperand::Double(number) => {
                            let rounded = round_double_to_place(number, place, operator == "$round");
                            Ok(BsonValue::Double(rounded))
                        }
                        ArithmeticOperand::DateTime(_) => unreachable!("numeric_operand rejects dates"),
                    }
                }
                "$concat" => {
                    let BsonValue::Array(arguments) = argument
                    else {
                        return Err(CommandError::new(2, "$concat requires an array of expressions"));
                    };
                    let mut joined = String::new();
                    for expression in arguments {
                        match evaluate_expression_vars(document, expression, variables)? {
                            BsonValue::String(text) => joined.push_str(&text),
                            // "$concat returns null if any argument is null or missing"
                            BsonValue::Null => return Ok(BsonValue::Null),
                            other => {
                                return Err(CommandError::new(2, format!("$concat requires string arguments, got {other:?}")));
                            }
                        }
                    }
                    Ok(BsonValue::String(joined))
                }
                "$toLower" | "$toUpper" | "$strLenCP" | "$strLenBytes" | "$substr" | "$substrBytes" | "$substrCP" | "$split" | "$trim"
                | "$ltrim" | "$rtrim" | "$indexOfCP" | "$indexOfBytes" | "$replaceOne" | "$replaceAll" | "$strcasecmp" => {
                    evaluate_string_operator(document, operator, argument, variables)
                }
                "$isArray" | "$arrayElemAt" | "$first" | "$last" | "$concatArrays" | "$reverseArray" | "$indexOfArray" | "$range"
                | "$slice" => evaluate_array_operator(document, operator, argument, variables),
                "$year" | "$month" | "$dayOfMonth" | "$hour" | "$minute" | "$second" | "$millisecond" | "$dayOfWeek" | "$dayOfYear"
                | "$week" | "$isoWeek" | "$isoWeekYear" | "$isoDayOfWeek" | "$dateToString" | "$dateFromString" | "$dateToParts"
                | "$dateFromParts" | "$dateAdd" | "$dateSubtract" | "$dateDiff" | "$dateTrunc" => {
                    evaluate_date_operator(document, operator, argument, variables)
                }
                "$toString" | "$toInt" | "$toLong" | "$toDouble" | "$toDecimal" | "$toBool" | "$toObjectId" | "$toDate" | "$type"
                | "$convert" | "$isNumber" => evaluate_conversion_operator(document, operator, argument, variables),
                "$size" => {
                    let value = evaluate_expression_vars(document, argument, variables)?;
                    match value {
                        BsonValue::Array(items) => Ok(count_value(items.len() as i64)),
                        other => Err(CommandError::new(
                            17124,
                            format!("The argument to $size must be an array, but was of type: {}", bson_type_name(&other)),
                        )),
                    }
                }
                "$ifNull" => {
                    let BsonValue::Array(arguments) = argument
                    else {
                        return Err(CommandError::new(2, "$ifNull requires an array of expressions"));
                    };
                    if arguments.len() < 2 {
                        return Err(CommandError::new(2, "$ifNull requires at least two arguments"));
                    }
                    for expression in arguments {
                        let value = evaluate_expression_vars(document, expression, variables)?;
                        if !matches!(value, BsonValue::Null) {
                            return Ok(value);
                        }
                    }
                    Ok(BsonValue::Null)
                }
                other => Err(CommandError::new(
                    59,
                    format!(
                        "unknown $project expression: {other} (supported: $add, $subtract, $multiply, $divide, $mod, $pow, $abs, $ceil, $floor, $round, $trunc, $sqrt, $concat, $toLower, $toUpper, $strLenCP, $strLenBytes, $substr, $substrBytes, $substrCP, $split, $trim, $ltrim, $rtrim, $indexOfCP, $indexOfBytes, $replaceOne, $replaceAll, $strcasecmp, $isArray, $arrayElemAt, $first, $last, $concatArrays, $reverseArray, $indexOfArray, $range, $slice, $year, $month, $dayOfMonth, $hour, $minute, $second, $millisecond, $dayOfWeek, $dayOfYear, $week, $isoWeek, $isoWeekYear, $isoDayOfWeek, $dateToString, $dateFromString, $dateToParts, $dateFromParts, $dateAdd, $dateSubtract, $dateDiff, $dateTrunc, $toString, $toInt, $toLong, $toDouble, $toDecimal, $toBool, $toObjectId, $toDate, $type, $convert, $isNumber, $cond, $ifNull, $size, $literal, $eq, $ne, $gt, $gte, $lt, $lte, $in, $and, $or, $not)"
                    ),
                )),
            }
        }
        BsonValue::Document(fields) => {
            // A document whose single field is not an operator is a literal
            // object whose values are themselves expressions; a missing field
            // is omitted rather than rendered as null.
            let mut result = BsonDocument::new();
            for (field, value) in &fields.fields {
                if let Some(evaluated) = evaluate_expression_optional_vars(document, value, variables)? {
                    set_field_path(&mut result, field, evaluated);
                }
            }
            Ok(BsonValue::Document(result))
        }
        other => Ok(other.clone()),
    }
}

/// 算术算子归一化后的操作数。
///
/// `Null` 同时覆盖 `null` 与已废弃的 `undefined`；`DateTime` 只在 `$add` 与
/// `$subtract` 里合法。Decimal128 的精确十进制算术尚未实现，按不支持处理。
pub(crate) enum ArithmeticOperand {
    /// `null` 或 `undefined`，算术结果直接是 `null`。
    Null,
    /// 整数操作数；`long` 标记原始 BSON 类型是否为 64 位。
    Integer {
        /// 以 `i128` 承载，避免 `int` / `long` 相加时中途丢失精度。
        value: i128,
        /// 原始类型是 `long` 时，结果只能落在 `long`。
        long: bool,
    },
    /// 双精度操作数。
    Double(f64),
    /// BSON Date，自 Unix 纪元起的毫秒数。
    DateTime(i64),
}

impl ArithmeticOperand {
    /// 排除 `null` 与日期之后的 `f64` 视图，供 `$divide` / `$mod` / `$pow` 使用。
    fn as_double(&self) -> Option<f64> {
        match self {
            Self::Integer { value, .. } => Some(*value as f64),
            Self::Double(number) => Some(*number),
            Self::Null | Self::DateTime(_) => None,
        }
    }
}

/// 把表达式结果归一化成算术操作数。
///
/// 接受 `int`、`long`、`double` 与 `date`，其余类型按 `mongod` 的
/// `TypeMismatch` 拒绝。错误码与措辞待真实 `mongod 8.0.9` 录制核对。
pub(crate) fn arithmetic_operand(value: &BsonValue, operator: &str) -> Result<ArithmeticOperand, CommandError> {
    Ok(match value {
        BsonValue::Null | BsonValue::Undefined => ArithmeticOperand::Null,
        BsonValue::Int32(number) => ArithmeticOperand::Integer { value: i128::from(*number), long: false },
        BsonValue::Int64(number) => ArithmeticOperand::Integer { value: i128::from(*number), long: true },
        BsonValue::Double(number) => ArithmeticOperand::Double(*number),
        BsonValue::DateTime(milliseconds) => ArithmeticOperand::DateTime(*milliseconds),
        other => {
            return Err(CommandError::new(16554, format!("{operator} only supports numeric or date types, not {}", bson_type_name(other))));
        }
    })
}

/// 只接受数值的算术算子（乘、除、取模、乘方、取整）使用的入口。
pub(crate) fn numeric_operand(value: &BsonValue, operator: &str) -> Result<ArithmeticOperand, CommandError> {
    match value {
        BsonValue::DateTime(_) => Err(CommandError::new(16555, format!("{operator} only supports numeric types, not date"))),
        BsonValue::Null | BsonValue::Undefined | BsonValue::Int32(_) | BsonValue::Int64(_) | BsonValue::Double(_) => {
            arithmetic_operand(value, operator)
        }
        other => Err(CommandError::new(16555, format!("{operator} only supports numeric types, not {}", bson_type_name(other)))),
    }
}

/// 把精确整数结果按最小可用的 BSON 整数类型写出。
///
/// 两侧都曾是 `int` 时优先 `int32`，放不下升到 `int64`；任一侧是 `long` 时
/// 只允许 `int64`，溢出即报错。
pub(crate) fn integer_arithmetic_result(value: i128, long: bool, operator: &str) -> Result<BsonValue, CommandError> {
    if !long && let Ok(small) = i32::try_from(value) {
        return Ok(BsonValue::Int32(small));
    }
    i64::try_from(value).map(BsonValue::Int64).map_err(|_| CommandError::new(2, format!("{operator} result overflows a 64-bit integer")))
}

/// 实现 `$add`、`$subtract`、`$multiply`、`$divide`、`$mod` 与 `$pow`。
///
/// 数值提升规则：任一操作数为 `double` 时结果为 `double`；两侧都是整数时保持
/// 整数，`int32` 放不下才升到 `int64`，`int64` 溢出报错。日期只参与 `$add`
/// 与 `$subtract`，且偏移量必须是整毫秒。所有错误码、错误措辞与 `$round`
/// 的浮点精度均待真实 `mongod 8.0.9` 录制核对。
pub(crate) fn evaluate_arithmetic(
    document: &BsonDocument,
    operator: &str,
    argument: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<BsonValue, CommandError> {
    let BsonValue::Array(arguments) = argument
    else {
        return Err(CommandError::new(2, format!("{operator} requires an array of expressions")));
    };
    let mut values = Vec::with_capacity(arguments.len());
    for expression in arguments {
        values.push(evaluate_expression_vars(document, expression, variables)?);
    }
    match operator {
        "$add" => {
            let mut integer_total: i128 = 0;
            let mut long = false;
            let mut double_total: Option<f64> = None;
            let mut date: Option<i64> = None;
            for value in &values {
                match arithmetic_operand(value, operator)? {
                    ArithmeticOperand::Null => return Ok(BsonValue::Null),
                    ArithmeticOperand::Integer { value, long: operand_long } => {
                        integer_total += value;
                        long |= operand_long;
                    }
                    ArithmeticOperand::Double(number) => *double_total.get_or_insert(0.0) += number,
                    ArithmeticOperand::DateTime(milliseconds) => {
                        if date.replace(milliseconds).is_some() {
                            return Err(CommandError::new(2, "$add cannot add two dates"));
                        }
                    }
                }
            }
            if let Some(milliseconds) = date {
                let offset = match double_total {
                    Some(number) if number.fract() == 0.0 && number >= i64::MIN as f64 && number <= i64::MAX as f64 => {
                        integer_total + number as i128
                    }
                    Some(_) => return Err(CommandError::new(2, "$add cannot add a non-integral value to a date")),
                    None => integer_total,
                };
                let shifted = i64::try_from(offset).ok().and_then(|offset| milliseconds.checked_add(offset));
                return shifted.map(BsonValue::DateTime).ok_or_else(|| CommandError::new(2, "$add date result is out of range"));
            }
            Ok(match double_total {
                Some(total) => BsonValue::Double(total + integer_total as f64),
                None => integer_arithmetic_result(integer_total, long, operator)?,
            })
        }
        "$subtract" => {
            if values.len() != 2 {
                return Err(CommandError::new(2, "$subtract requires exactly two expressions"));
            }
            let left = arithmetic_operand(&values[0], operator)?;
            let right = arithmetic_operand(&values[1], operator)?;
            match (left, right) {
                (ArithmeticOperand::Null, _) | (_, ArithmeticOperand::Null) => Ok(BsonValue::Null),
                (ArithmeticOperand::DateTime(left), ArithmeticOperand::DateTime(right)) => {
                    left.checked_sub(right).map(BsonValue::Int64).ok_or_else(|| CommandError::new(2, "$subtract date result is out of range"))
                }
                (ArithmeticOperand::DateTime(left), ArithmeticOperand::Integer { value, .. }) => i64::try_from(value)
                    .ok()
                    .and_then(|offset| left.checked_sub(offset))
                    .map(BsonValue::DateTime)
                    .ok_or_else(|| CommandError::new(2, "$subtract date result is out of range")),
                (ArithmeticOperand::DateTime(left), ArithmeticOperand::Double(number)) => integral_i64(&BsonValue::Double(number))
                    .and_then(|offset| left.checked_sub(offset))
                    .map(BsonValue::DateTime)
                    .ok_or_else(|| CommandError::new(2, "$subtract cannot subtract a non-integral value from a date")),
                (ArithmeticOperand::Integer { .. } | ArithmeticOperand::Double(_), ArithmeticOperand::DateTime(_)) => {
                    Err(CommandError::new(2, "$subtract cannot subtract a date from a number"))
                }
                (
                    ArithmeticOperand::Integer { value: left, long: left_long },
                    ArithmeticOperand::Integer { value: right, long: right_long },
                ) => integer_arithmetic_result(left - right, left_long || right_long, operator),
                (ArithmeticOperand::Integer { value, .. }, ArithmeticOperand::Double(right)) => Ok(BsonValue::Double(value as f64 - right)),
                (ArithmeticOperand::Double(left), ArithmeticOperand::Integer { value, .. }) => Ok(BsonValue::Double(left - value as f64)),
                (ArithmeticOperand::Double(left), ArithmeticOperand::Double(right)) => Ok(BsonValue::Double(left - right)),
            }
        }
        "$multiply" => {
            if values.is_empty() {
                return Err(CommandError::new(2, "$multiply requires at least one expression"));
            }
            let mut integer_total: i128 = 1;
            let mut long = false;
            let mut double_total: Option<f64> = None;
            for value in &values {
                match numeric_operand(value, operator)? {
                    ArithmeticOperand::Null => return Ok(BsonValue::Null),
                    ArithmeticOperand::Integer { value, long: operand_long } => {
                        integer_total = integer_total
                            .checked_mul(value)
                            .ok_or_else(|| CommandError::new(2, "$multiply result overflows a 64-bit integer"))?;
                        long |= operand_long;
                    }
                    ArithmeticOperand::Double(number) => *double_total.get_or_insert(1.0) *= number,
                    ArithmeticOperand::DateTime(_) => unreachable!("numeric_operand rejects dates"),
                }
            }
            Ok(match double_total {
                Some(total) => BsonValue::Double(total * integer_total as f64),
                None => integer_arithmetic_result(integer_total, long, operator)?,
            })
        }
        "$divide" => {
            if values.len() != 2 {
                return Err(CommandError::new(2, "$divide requires exactly two expressions"));
            }
            let left = numeric_operand(&values[0], operator)?;
            let right = numeric_operand(&values[1], operator)?;
            if matches!(&left, ArithmeticOperand::Null) || matches!(&right, ArithmeticOperand::Null) {
                return Ok(BsonValue::Null);
            }
            let divisor = right.as_double().expect("non-null numeric operand");
            if divisor == 0.0 {
                return Err(CommandError::new(2, "can't $divide by zero"));
            }
            let dividend = left.as_double().expect("non-null numeric operand");
            Ok(BsonValue::Double(dividend / divisor))
        }
        "$mod" => {
            if values.len() != 2 {
                return Err(CommandError::new(2, "$mod requires exactly two expressions"));
            }
            let left = numeric_operand(&values[0], operator)?;
            let right = numeric_operand(&values[1], operator)?;
            match (&left, &right) {
                (ArithmeticOperand::Null, _) | (_, ArithmeticOperand::Null) => Ok(BsonValue::Null),
                (
                    ArithmeticOperand::Integer { value: left, long: left_long },
                    ArithmeticOperand::Integer { value: right, long: right_long },
                ) => {
                    if *right == 0 {
                        return Err(CommandError::new(2, "can't $mod by zero"));
                    }
                    integer_arithmetic_result(left % right, *left_long || *right_long, operator)
                }
                _ => {
                    let divisor = right.as_double().expect("non-null numeric operand");
                    if divisor == 0.0 {
                        return Err(CommandError::new(2, "can't $mod by zero"));
                    }
                    let dividend = left.as_double().expect("non-null numeric operand");
                    Ok(BsonValue::Double(dividend % divisor))
                }
            }
        }
        "$pow" => {
            if values.len() != 2 {
                return Err(CommandError::new(2, "$pow requires exactly two expressions"));
            }
            let base = numeric_operand(&values[0], operator)?;
            let exponent = numeric_operand(&values[1], operator)?;
            match (&base, &exponent) {
                (ArithmeticOperand::Null, _) | (_, ArithmeticOperand::Null) => Ok(BsonValue::Null),
                (ArithmeticOperand::Integer { value: base, long }, ArithmeticOperand::Integer { value: exponent, .. }) if *exponent >= 0 => {
                    integer_power(*base, *exponent, *long, operator)
                }
                _ => {
                    let base = base.as_double().expect("non-null numeric operand");
                    let exponent = exponent.as_double().expect("non-null numeric operand");
                    if base == 0.0 && exponent < 0.0 {
                        return Err(CommandError::new(2, "can't take 0 to a negative exponent"));
                    }
                    Ok(BsonValue::Double(base.powf(exponent)))
                }
            }
        }
        other => Err(CommandError::new(59, format!("unknown arithmetic expression: {other}"))),
    }
}

/// 整数乘方：只处理非负指数，结果按整数类型提升规则写出。
pub(crate) fn integer_power(base: i128, exponent: i128, long: bool, operator: &str) -> Result<BsonValue, CommandError> {
    let result = match base {
        0 if exponent == 0 => 1,
        0 => 0,
        1 => 1,
        -1 if exponent % 2 == 0 => 1,
        -1 => -1,
        _ => {
            // 绝对值大于 1 时，指数超过 63 必然溢出 64 位整数。
            if exponent > 63 {
                return Err(CommandError::new(2, format!("{operator} result overflows a 64-bit integer")));
            }
            let mut result: i128 = 1;
            for _ in 0..exponent {
                result =
                    result.checked_mul(base).ok_or_else(|| CommandError::new(2, format!("{operator} result overflows a 64-bit integer")))?;
            }
            result
        }
    };
    integer_arithmetic_result(result, long, operator)
}

/// 整数按 `10^-place` 取舍；`round` 为 `false` 时向零截断。
///
/// `$round` 用半偶数（银行家）舍入，与 `mongod` 的整数路径一致。
pub(crate) fn round_integer_to_place(value: i128, place: i32, round: bool) -> i128 {
    if place >= 0 {
        return value;
    }
    let factor = 10i128.pow((-place) as u32);
    let quotient = value / factor;
    if !round {
        return quotient * factor;
    }
    let remainder = value % factor;
    let doubled = remainder.abs() * 2;
    let step = if doubled > factor || (doubled == factor && quotient % 2 != 0) { value.signum() } else { 0 };
    (quotient + step) * factor
}

/// 双精度按 `10^-place` 取舍；`$round` 用半偶数舍入。
///
/// 缩放用的是二进制浮点，极端位数上的舍入可能与 `mongod` 不同，待真机核对。
pub(crate) fn round_double_to_place(value: f64, place: i32, round: bool) -> f64 {
    let factor = 10f64.powi(place);
    let scaled = value * factor;
    let rounded = if round { scaled.round_ties_even() } else { scaled.trunc() };
    rounded / factor
}

/// 字符串算子归一化后的输入。
pub(crate) enum StringArgument {
    /// `null` 或缺失；各算子自行决定按空串、`null` 还是报错处理。
    Null,
    /// 已求值的字符串。
    Text(String),
}

/// 把字符串算子的单个输入归一化。
///
/// 非字符串按 `TypeMismatch` 拒绝，各算子传入手册对应的错误码；错误码与措辞
/// 待真实 `mongod 8.0.9` 录制核对。
pub(crate) fn string_argument(value: BsonValue, operator: &str, code: i32) -> Result<StringArgument, CommandError> {
    match value {
        BsonValue::Null | BsonValue::Undefined => Ok(StringArgument::Null),
        BsonValue::String(text) => Ok(StringArgument::Text(text)),
        other => Err(CommandError::new(code, format!("{operator} requires a string argument, found: {}", bson_type_name(&other)))),
    }
}

/// 取算子的定长参数列表。
pub(crate) fn operator_arguments<'a>(
    argument: &'a BsonValue,
    operator: &str,
    minimum: usize,
    maximum: usize,
) -> Result<&'a [BsonValue], CommandError> {
    let BsonValue::Array(arguments) = argument
    else {
        return Err(CommandError::new(2, format!("{operator} requires an array of expressions")));
    };
    if arguments.len() < minimum || arguments.len() > maximum {
        return Err(CommandError::new(2, format!("{operator} requires between {minimum} and {maximum} expressions")));
    }
    Ok(arguments)
}

/// 取非负整数参数；浮点必须先有精确整数视图才接受。
pub(crate) fn non_negative_integer(value: &BsonValue, operator: &str, code: i32) -> Result<i64, CommandError> {
    match integral_i64(value) {
        Some(number) if number >= 0 => Ok(number),
        _ => Err(CommandError::new(code, format!("{operator} requires a non-negative integer argument"))),
    }
}

/// `$trim` / `$ltrim` / `$rtrim` 未指定 `chars` 时的默认字符集。
///
/// `mongod` 手册给的默认集合包含空字符、空格、制表、回车、换行、垂直制表与换页。
pub(crate) fn default_trim_chars() -> String {
    "\u{0} \t\n\r\u{b}\u{c}".to_string()
}

/// 求值字符串类聚合算子。
///
/// 语义按 `mongod` 手册实现：`$toLower` / `$toUpper` 把 `null` 当空串，
/// `$strcasecmp` 把 `null` 当空串参与比较，其余长度、截取、拆分、裁剪、查找、
/// 替换类算子遇到 `null` 直接返回 `null`。所有错误码、错误措辞、非 ASCII 大小写
/// 映射与 UTF-8 边界判定均待真实 `mongod 8.0.9` 录制核对。
pub(crate) fn evaluate_string_operator(
    document: &BsonDocument,
    operator: &str,
    argument: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<BsonValue, CommandError> {
    match operator {
        "$toLower" | "$toUpper" => {
            let value = evaluate_expression_vars(document, argument, variables)?;
            let text = match string_argument(value, operator, 16007)? {
                StringArgument::Null => String::new(),
                StringArgument::Text(text) => text,
            };
            Ok(BsonValue::String(if operator == "$toLower" { text.to_lowercase() } else { text.to_uppercase() }))
        }
        "$strLenCP" | "$strLenBytes" => {
            let value = evaluate_expression_vars(document, argument, variables)?;
            let code = if operator == "$strLenCP" { 34471 } else { 34473 };
            match string_argument(value, operator, code)? {
                StringArgument::Null => Ok(BsonValue::Null),
                StringArgument::Text(text) => {
                    let length = if operator == "$strLenCP" { text.chars().count() } else { text.len() };
                    Ok(count_value(length as i64))
                }
            }
        }
        "$substr" | "$substrBytes" | "$substrCP" => {
            let arguments = operator_arguments(argument, operator, 3, 3)?;
            let value = evaluate_expression_vars(document, &arguments[0], variables)?;
            let text = match string_argument(value, operator, 34450)? {
                StringArgument::Null => return Ok(BsonValue::Null),
                StringArgument::Text(text) => text,
            };
            let start = evaluate_expression_vars(document, &arguments[1], variables)?;
            let start =
                integral_i64(&start).ok_or_else(|| CommandError::new(34452, format!("{operator} requires an integer starting index")))?;
            let length = evaluate_expression_vars(document, &arguments[2], variables)?;
            let length = integral_i64(&length).ok_or_else(|| CommandError::new(34453, format!("{operator} requires an integer length")))?;
            if start < 0 {
                // 负起点按手册返回空串。
                return Ok(BsonValue::String(String::new()));
            }
            if operator == "$substrCP" {
                Ok(BsonValue::String(substring_by_code_point(&text, start as usize, length)))
            }
            else {
                substring_by_byte(&text, start as usize, length, operator)
            }
        }
        "$split" => {
            let arguments = operator_arguments(argument, operator, 2, 2)?;
            let input = evaluate_expression_vars(document, &arguments[0], variables)?;
            let separator = evaluate_expression_vars(document, &arguments[1], variables)?;
            let input = string_argument(input, operator, 40085)?;
            let separator = string_argument(separator, operator, 40086)?;
            match (input, separator) {
                (StringArgument::Null, _) | (_, StringArgument::Null) => Ok(BsonValue::Null),
                (StringArgument::Text(input), StringArgument::Text(separator)) => {
                    if separator.is_empty() {
                        return Err(CommandError::new(40086, "$split requires a non-empty separator"));
                    }
                    Ok(BsonValue::Array(input.split(separator.as_str()).map(|part| BsonValue::String(part.to_string())).collect()))
                }
            }
        }
        "$trim" | "$ltrim" | "$rtrim" => {
            let (input_expression, chars_expression) = match argument {
                BsonValue::Document(options) => {
                    for (field, _) in &options.fields {
                        if field != "input" && field != "chars" {
                            return Err(CommandError::new(2, format!("{operator} found an unknown argument: {field}")));
                        }
                    }
                    let input = options.get("input").ok_or_else(|| CommandError::new(2, format!("{operator} requires an 'input' field")))?;
                    (input, options.get("chars"))
                }
                other => (other, None),
            };
            let input = evaluate_expression_vars(document, input_expression, variables)?;
            let input = match string_argument(input, operator, 50699)? {
                StringArgument::Null => return Ok(BsonValue::Null),
                StringArgument::Text(text) => text,
            };
            let chars = match chars_expression {
                Some(expression) => {
                    let chars = evaluate_expression_vars(document, expression, variables)?;
                    match string_argument(chars, operator, 50702)? {
                        // `chars` 为 `null` 时按未提供处理，用默认字符集，待真机核对。
                        StringArgument::Null => default_trim_chars(),
                        StringArgument::Text(text) => text,
                    }
                }
                None => default_trim_chars(),
            };
            let trimmed = match operator {
                "$trim" => input.trim_matches(|character| chars.contains(character)),
                "$ltrim" => input.trim_start_matches(|character| chars.contains(character)),
                _ => input.trim_end_matches(|character| chars.contains(character)),
            };
            Ok(BsonValue::String(trimmed.to_string()))
        }
        "$indexOfCP" | "$indexOfBytes" => {
            let arguments = operator_arguments(argument, operator, 2, 4)?;
            let input = evaluate_expression_vars(document, &arguments[0], variables)?;
            let search = evaluate_expression_vars(document, &arguments[1], variables)?;
            let input = string_argument(input, operator, 40091)?;
            let search = string_argument(search, operator, 40092)?;
            let (input, search) = match (input, search) {
                (StringArgument::Null, _) | (_, StringArgument::Null) => return Ok(BsonValue::Null),
                (StringArgument::Text(input), StringArgument::Text(search)) => (input, search),
            };
            let start = match arguments.get(2) {
                Some(expression) => non_negative_integer(&evaluate_expression_vars(document, expression, variables)?, operator, 40093)?,
                None => 0,
            };
            if operator == "$indexOfCP" {
                index_of_code_point(&input, &search, start, arguments.get(3), document, variables)
            }
            else {
                index_of_byte(&input, &search, start, arguments.get(3), document, variables)
            }
        }
        "$replaceOne" | "$replaceAll" => {
            let arguments = operator_arguments(argument, operator, 3, 3)?;
            let input = evaluate_expression_vars(document, &arguments[0], variables)?;
            let find = evaluate_expression_vars(document, &arguments[1], variables)?;
            let replacement = evaluate_expression_vars(document, &arguments[2], variables)?;
            let input = string_argument(input, operator, 51745)?;
            let find = string_argument(find, operator, 51746)?;
            let replacement = string_argument(replacement, operator, 51747)?;
            match (input, find, replacement) {
                (StringArgument::Null, _, _) | (_, StringArgument::Null, _) | (_, _, StringArgument::Null) => Ok(BsonValue::Null),
                (StringArgument::Text(input), StringArgument::Text(find), StringArgument::Text(replacement)) => {
                    if find.is_empty() {
                        // 空 `find` 按手册原样返回输入串。
                        return Ok(BsonValue::String(input));
                    }
                    Ok(BsonValue::String(if operator == "$replaceOne" {
                        input.replacen(find.as_str(), replacement.as_str(), 1)
                    }
                    else {
                        input.replace(find.as_str(), replacement.as_str())
                    }))
                }
            }
        }
        "$strcasecmp" => {
            let arguments = operator_arguments(argument, operator, 2, 2)?;
            let left = evaluate_expression_vars(document, &arguments[0], variables)?;
            let right = evaluate_expression_vars(document, &arguments[1], variables)?;
            let text = |value: BsonValue| -> Result<String, CommandError> {
                match string_argument(value, "$strcasecmp", 16007)? {
                    StringArgument::Null => Ok(String::new()),
                    StringArgument::Text(text) => Ok(text),
                }
            };
            let left = text(left)?;
            let right = text(right)?;
            Ok(BsonValue::Int32(match left.to_lowercase().cmp(&right.to_lowercase()) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            }))
        }
        other => unreachable!("evaluate_string_operator called with {other}"),
    }
}

/// 按字节截取；起点与终点都必须落在 UTF-8 字符边界上。
///
/// 负 `length` 表示取到串尾，越界一律截断到串尾；起始下标落在续字节上属于
/// 手册定义的范围错误，错误码与措辞待真机核对。
pub(crate) fn substring_by_byte(text: &str, start: usize, length: i64, operator: &str) -> Result<BsonValue, CommandError> {
    if start >= text.len() {
        return Ok(BsonValue::String(String::new()));
    }
    if !text.is_char_boundary(start) {
        return Err(CommandError::new(28656, format!("{operator}: invalid range, starting index is a UTF-8 continuation byte")));
    }
    let end = if length < 0 { text.len() } else { start.saturating_add(usize::try_from(length).unwrap_or(usize::MAX)).min(text.len()) };
    if !text.is_char_boundary(end) {
        return Err(CommandError::new(28657, format!("{operator}: invalid range, ending index is in the middle of a UTF-8 character")));
    }
    Ok(BsonValue::String(text[start..end].to_string()))
}

/// 按 Unicode 码点截取；负 `length` 表示取到串尾。
pub(crate) fn substring_by_code_point(text: &str, start: usize, length: i64) -> String {
    let count = text.chars().count();
    if start >= count {
        return String::new();
    }
    let end = if length < 0 { count } else { start.saturating_add(usize::try_from(length).unwrap_or(usize::MAX)).min(count) };
    text.chars().skip(start).take(end - start).collect()
}

/// `$indexOfCP`：在码点窗口内查找，返回码点下标。
pub(crate) fn index_of_code_point(
    input: &str,
    search: &str,
    start: i64,
    end_expression: Option<&BsonValue>,
    document: &BsonDocument,
    variables: &ExpressionVariables,
) -> Result<BsonValue, CommandError> {
    let haystack: Vec<char> = input.chars().collect();
    let needle: Vec<char> = search.chars().collect();
    let count = haystack.len() as i64;
    let end = match end_expression {
        Some(expression) => non_negative_integer(&evaluate_expression_vars(document, expression, variables)?, "$indexOfCP", 40094)?,
        None => count,
    };
    if start > count || start > end || needle.len() as i64 > count {
        return Ok(BsonValue::Int32(-1));
    }
    let end = end.min(count) as usize;
    let start = start as usize;
    let window = &haystack[start..end];
    let position = if needle.is_empty() { Some(0) } else { window.windows(needle.len()).position(|candidate| candidate == needle.as_slice()) };
    Ok(count_value(position.map_or(-1, |offset| (start + offset) as i64)))
}

/// `$indexOfBytes`：在字节窗口内查找，返回字节下标。
///
/// 起点与终点必须落在 UTF-8 字符边界上，否则按手册报范围错误；错误码与措辞
/// 待真机核对。
pub(crate) fn index_of_byte(
    input: &str,
    search: &str,
    start: i64,
    end_expression: Option<&BsonValue>,
    document: &BsonDocument,
    variables: &ExpressionVariables,
) -> Result<BsonValue, CommandError> {
    let count = input.len() as i64;
    let end = match end_expression {
        Some(expression) => non_negative_integer(&evaluate_expression_vars(document, expression, variables)?, "$indexOfBytes", 40094)?,
        None => count,
    };
    if start > count || start > end {
        return Ok(BsonValue::Int32(-1));
    }
    let start = start as usize;
    let end = end.min(count) as usize;
    if !input.is_char_boundary(start) || !input.is_char_boundary(end) {
        return Err(CommandError::new(40095, "$indexOfBytes requires a starting and ending index on UTF-8 character boundaries"));
    }
    Ok(count_value(input[start..end].find(search).map_or(-1, |offset| (start + offset) as i64)))
}

/// 数组算子归一化后的输入。
pub(crate) enum ArrayArgument {
    /// `null` 或缺失，多数数组算子直接返回 `null`。
    Null,
    /// 已求值的数组元素。
    Items(Vec<BsonValue>),
}

/// 把数组算子的单个输入归一化。
///
/// 非数组按 `TypeMismatch` 拒绝，各算子传入手册对应的错误码；错误码与措辞
/// 待真实 `mongod 8.0.9` 录制核对。
pub(crate) fn array_argument(value: BsonValue, operator: &str, code: i32) -> Result<ArrayArgument, CommandError> {
    match value {
        BsonValue::Null | BsonValue::Undefined => Ok(ArrayArgument::Null),
        BsonValue::Array(items) => Ok(ArrayArgument::Items(items)),
        other => Err(CommandError::new(code, format!("{operator} requires an array argument, found: {}", bson_type_name(&other)))),
    }
}

/// 取数组元素；越界返回 `null`。
///
/// `mongod` 对越界返回缺失字段（投影时字段消失），本仓暂以 `null` 表达，
/// 差异待真机录制后决定是否需要独立的缺失值。
pub(crate) fn array_element(items: &[BsonValue], position: i64) -> BsonValue {
    usize::try_from(position).ok().and_then(|index| items.get(index)).cloned().unwrap_or(BsonValue::Null)
}

/// 求值数组类聚合算子。
///
/// 语义按 `mongod` 手册实现：`null` 输入一律返回 `null`，越界取元素与空数组
/// 的 `$first` / `$last` 在 `mongod` 里是缺失字段，这里暂以 `null` 表达。
/// 所有错误码与错误措辞均待真实 `mongod 8.0.9` 录制核对。
pub(crate) fn evaluate_array_operator(
    document: &BsonDocument,
    operator: &str,
    argument: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<BsonValue, CommandError> {
    match operator {
        "$isArray" => {
            let value = evaluate_expression_vars(document, argument, variables)?;
            Ok(BsonValue::Bool(matches!(value, BsonValue::Array(_))))
        }
        "$arrayElemAt" | "$first" | "$last" => {
            let (value, index) = if operator == "$arrayElemAt" {
                let arguments = operator_arguments(argument, operator, 2, 2)?;
                (evaluate_expression_vars(document, &arguments[0], variables)?, Some(&arguments[1]))
            }
            else {
                (evaluate_expression_vars(document, argument, variables)?, None)
            };
            let items = match array_argument(value, operator, 28689)? {
                ArrayArgument::Null => return Ok(BsonValue::Null),
                ArrayArgument::Items(items) => items,
            };
            let position = match index {
                Some(expression) => {
                    let index = evaluate_expression_vars(document, expression, variables)?;
                    integral_i64(&index).ok_or_else(|| CommandError::new(28690, format!("{operator}'s second argument must be an integer")))?
                }
                None if operator == "$first" => 0,
                None => items.len() as i64 - 1,
            };
            let position = if position < 0 { (items.len() as i64).saturating_add(position) } else { position };
            Ok(array_element(&items, position))
        }
        "$concatArrays" => {
            let arguments = operator_arguments(argument, operator, 1, usize::MAX)?;
            let mut combined = Vec::new();
            for expression in arguments {
                let value = evaluate_expression_vars(document, expression, variables)?;
                match array_argument(value, operator, 28664)? {
                    ArrayArgument::Null => return Ok(BsonValue::Null),
                    ArrayArgument::Items(items) => combined.extend(items),
                }
            }
            Ok(BsonValue::Array(combined))
        }
        "$reverseArray" => {
            let value = evaluate_expression_vars(document, argument, variables)?;
            match array_argument(value, operator, 34435)? {
                ArrayArgument::Null => Ok(BsonValue::Null),
                ArrayArgument::Items(mut items) => {
                    items.reverse();
                    Ok(BsonValue::Array(items))
                }
            }
        }
        "$indexOfArray" => {
            let arguments = operator_arguments(argument, operator, 2, 4)?;
            let value = evaluate_expression_vars(document, &arguments[0], variables)?;
            let search = evaluate_expression_vars(document, &arguments[1], variables)?;
            let items = match array_argument(value, operator, 40090)? {
                ArrayArgument::Null => return Ok(BsonValue::Null),
                ArrayArgument::Items(items) => items,
            };
            let start = match arguments.get(2) {
                Some(expression) => non_negative_integer(&evaluate_expression_vars(document, expression, variables)?, operator, 40096)?,
                None => 0,
            };
            let end = match arguments.get(3) {
                Some(expression) => non_negative_integer(&evaluate_expression_vars(document, expression, variables)?, operator, 40097)?,
                None => items.len() as i64,
            };
            let end = end.min(items.len() as i64);
            if start > end {
                return Ok(BsonValue::Int32(-1));
            }
            let start = start as usize;
            let end = end as usize;
            let position = items[start..end].iter().position(|item| compare_values(item, &search) == Ordering::Equal);
            Ok(count_value(position.map_or(-1, |offset| (start + offset) as i64)))
        }
        "$range" => {
            let arguments = operator_arguments(argument, operator, 2, 3)?;
            let values: Vec<BsonValue> =
                arguments.iter().map(|expression| evaluate_expression_vars(document, expression, variables)).collect::<Result<_, _>>()?;
            let numbers = values
                .iter()
                .map(|value| integral_i64(value).ok_or_else(|| CommandError::new(34443, format!("{operator} requires integer arguments"))))
                .collect::<Result<Vec<_>, _>>()?;
            let start = numbers[0];
            let end = numbers[1];
            let step = numbers.get(2).copied().unwrap_or(1);
            if step == 0 {
                return Err(CommandError::new(34449, "$range requires a non-zero step value"));
            }
            let long = values.iter().any(|value| matches!(value, BsonValue::Int64(_)));
            let mut items = Vec::new();
            let mut current = start;
            while (step > 0 && current < end) || (step < 0 && current > end) {
                // 元素上限防止一条表达式把内存打满，具体上限待真机核对。
                if items.len() >= 1_000_000 {
                    return Err(CommandError::new(2, "$range result exceeds 1000000 elements"));
                }
                items.push(if long {
                    BsonValue::Int64(current)
                }
                else {
                    i32::try_from(current).map(BsonValue::Int32).unwrap_or(BsonValue::Int64(current))
                });
                current = current.checked_add(step).ok_or_else(|| CommandError::new(2, "$range step overflows a 64-bit integer"))?;
            }
            Ok(BsonValue::Array(items))
        }
        "$slice" => {
            let arguments = operator_arguments(argument, operator, 2, 3)?;
            let value = evaluate_expression_vars(document, &arguments[0], variables)?;
            let items = match array_argument(value, operator, 28724)? {
                ArrayArgument::Null => return Ok(BsonValue::Null),
                ArrayArgument::Items(items) => items,
            };
            let length = items.len() as i64;
            let integer = |expression: &BsonValue| -> Result<i64, CommandError> {
                let value = evaluate_expression_vars(document, expression, variables)?;
                integral_i64(&value).ok_or_else(|| CommandError::new(28729, format!("{operator} requires integer arguments")))
            };
            if arguments.len() == 2 {
                let count = integer(&arguments[1])?;
                let (start, end) = if count >= 0 { (0, count.min(length)) } else { ((length.saturating_add(count)).max(0), length) };
                Ok(BsonValue::Array(items[start as usize..end as usize].to_vec()))
            }
            else {
                let position = integer(&arguments[1])?;
                let count = integer(&arguments[2])?;
                if count <= 0 {
                    return Err(CommandError::new(28729, "$slice third argument must be positive"));
                }
                let start = if position < 0 { (length.saturating_add(position)).max(0) } else { position.min(length) };
                let end = start.saturating_add(count).min(length);
                Ok(BsonValue::Array(items[start as usize..end as usize].to_vec()))
            }
        }
        other => unreachable!("evaluate_array_operator called with {other}"),
    }
}

/// 日期算子解释时使用的固定偏移。
///
/// 本仓不内置 IANA 时区数据库：只支持 `UTC` / `GMT` / `Z` 与 `±HH[:MM]` 形式的
/// 固定偏移。`Asia/Shanghai` 一类的 Olson 名称一律拒绝，夏令时与历史时区规则
/// 的边界语义待真实 `mongod 8.0.9` 录制核对。
#[derive(Clone, Copy)]
pub(crate) struct FixedOffset {
    pub(crate) seconds: i64,
}

impl FixedOffset {
    fn utc() -> Self {
        Self { seconds: 0 }
    }

    fn parse(value: &str) -> Result<Self, CommandError> {
        let trimmed = value.trim();
        let upper = trimmed.to_ascii_uppercase();
        if matches!(upper.as_str(), "UTC" | "GMT" | "Z") {
            return Ok(Self::utc());
        }
        let sign = match upper.as_bytes().first() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            _ => return Err(invalid_timezone(value)),
        };
        let rest = &upper[1..];
        let (hours, minutes) = match rest.len() {
            2 => (rest, "00"),
            4 => (&rest[..2], &rest[2..]),
            5 if rest.as_bytes()[2] == b':' => (&rest[..2], &rest[3..]),
            _ => return Err(invalid_timezone(value)),
        };
        let parse_part = |part: &str| part.parse::<i64>().map_err(|_| invalid_timezone(value));
        let hours = parse_part(hours)?;
        let minutes = parse_part(minutes)?;
        if hours > 23 || minutes > 59 {
            return Err(invalid_timezone(value));
        }
        Ok(Self { seconds: sign * (hours * 3600 + minutes * 60) })
    }

    /// `+HHMM` 形式的文本，供 `%z` 使用。
    fn text(self) -> String {
        let sign = if self.seconds < 0 { '-' } else { '+' };
        let total = self.seconds.abs();
        format!("{sign}{:02}{:02}", total / 3600, (total % 3600) / 60)
    }
}

pub(crate) fn invalid_timezone(value: &str) -> CommandError {
    CommandError::new(
        40485,
        format!(
            "unrecognized time zone identifier: \"{value}\"; this facade supports UTC and fixed ±HH:MM offsets only (IANA zone rules pending real mongod verification)"
        ),
    )
}

/// 日期记录：UTC 毫秒与解释用的固定偏移。
#[derive(Clone, Copy)]
pub(crate) struct DateValue {
    pub(crate) millis: i64,
    pub(crate) offset: FixedOffset,
}

/// 从算子参数文档里取可选的 `timezone`。
pub(crate) fn evaluate_timezone(
    document: &BsonDocument,
    options: &BsonDocument,
    variables: &ExpressionVariables,
) -> Result<FixedOffset, CommandError> {
    match options.get("timezone") {
        None | Some(BsonValue::Null) => Ok(FixedOffset::utc()),
        Some(expression) => match evaluate_expression_vars(document, expression, variables)? {
            BsonValue::String(name) => FixedOffset::parse(&name),
            other => Err(CommandError::new(40517, format!("timezone must evaluate to a string, found: {}", bson_type_name(&other)))),
        },
    }
}

/// 把 BSON 值解释为 UTC 毫秒；`null` 与缺失返回 `None`。
///
/// Date、Timestamp 与 ObjectId 都按手册接受；错误码与措辞待真机核对。
pub(crate) fn date_millis(value: &BsonValue, operator: &str) -> Result<Option<i64>, CommandError> {
    match value {
        BsonValue::Null | BsonValue::Undefined => Ok(None),
        BsonValue::DateTime(millis) => Ok(Some(*millis)),
        BsonValue::Timestamp { t, .. } => Ok(Some(i64::from(*t) * 1000)),
        BsonValue::ObjectId(bytes) => Ok(Some(i64::from(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])) * 1000)),
        other => Err(CommandError::new(16006, format!("{operator} only supports Date, Timestamp, or ObjectId, not {}", bson_type_name(other)))),
    }
}

/// 字段类日期算子的参数：既可以是裸日期表达式，也可以是 `{ date, timezone }`。
pub(crate) fn evaluate_date_argument(
    document: &BsonDocument,
    argument: &BsonValue,
    variables: &ExpressionVariables,
    operator: &str,
) -> Result<Option<DateValue>, CommandError> {
    if let BsonValue::Document(options) = argument
        && let Some(date) = options.get("date")
    {
        let offset = evaluate_timezone(document, options, variables)?;
        let value = evaluate_expression_vars(document, date, variables)?;
        return Ok(date_millis(&value, operator)?.map(|millis| DateValue { millis, offset }));
    }
    let value = evaluate_expression_vars(document, argument, variables)?;
    Ok(date_millis(&value, operator)?.map(|millis| DateValue { millis, offset: FixedOffset::utc() }))
}

/// UTC 民用日期时间，不携带时区偏移。
#[derive(Clone, Copy)]
pub(crate) struct CivilDateTime {
    pub(crate) year: i64,
    pub(crate) month: u32,
    pub(crate) day: u32,
    pub(crate) hour: u32,
    pub(crate) minute: u32,
    pub(crate) second: u32,
    pub(crate) millisecond: u32,
}

/// 把本地偏移加到 UTC 毫秒上，供民用历字段计算使用。
pub(crate) fn local_millis(value: DateValue) -> Result<i64, CommandError> {
    let offset =
        value.offset.seconds.checked_mul(1000).ok_or_else(|| CommandError::new(2, "timezone offset overflows a 64-bit millisecond count"))?;
    value.millis.checked_add(offset).ok_or_else(|| CommandError::new(2, "date arithmetic overflows a 64-bit millisecond count"))
}

/// 公历日期转自 Unix 纪元起的天数，供日期算子复用既有的民用历换算。
pub(crate) fn date_days(year: i64, month: u32, day: u32) -> i64 {
    days_from_civil(year, i64::from(month), i64::from(day))
}

impl CivilDateTime {
    fn from_millis(millis: i64) -> Self {
        let days = millis.div_euclid(86_400_000);
        let within = millis.rem_euclid(86_400_000);
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month: month as u32,
            day: day as u32,
            hour: (within / 3_600_000) as u32,
            minute: ((within / 60_000) % 60) as u32,
            second: ((within / 1_000) % 60) as u32,
            millisecond: (within % 1_000) as u32,
        }
    }

    fn to_millis(self) -> i64 {
        date_days(self.year, self.month, self.day) * 86_400_000
            + i64::from(self.hour) * 3_600_000
            + i64::from(self.minute) * 60_000
            + i64::from(self.second) * 1_000
            + i64::from(self.millisecond)
    }

    fn days(self) -> i64 {
        date_days(self.year, self.month, self.day)
    }

    fn day_of_year(self) -> i64 {
        self.days() - date_days(self.year, 1, 1) + 1
    }

    /// Sunday 记 1 的星期序号。
    fn day_of_week(self) -> i64 {
        sunday_day_of_week(self.days())
    }

    /// ISO 星期序号，Monday 记 1。
    fn iso_day_of_week(self) -> i64 {
        iso_day_of_week(self.days())
    }

    /// Sunday 记 1 的周序号，年初不足一周的部分记 0。
    fn week_of_year(self) -> i64 {
        (self.day_of_year() - 1 + 7 - self.day_of_week()) / 7
    }

    /// ISO 周年与周序号。
    fn iso_week(self) -> (i64, i64) {
        let thursday = self.days() - (self.iso_day_of_week() - 4);
        let (iso_year, _, _) = civil_from_days(thursday);
        let january_fourth = days_from_civil(iso_year, 1, 4);
        let first_thursday = january_fourth + (4 - iso_day_of_week(january_fourth));
        (iso_year, (thursday - first_thursday) / 7 + 1)
    }
}

/// Sunday 记 1 的星期序号。
pub(crate) fn sunday_day_of_week(days: i64) -> i64 {
    (days + 4).rem_euclid(7) + 1
}

/// ISO 星期序号，Monday 记 1。
pub(crate) fn iso_day_of_week(days: i64) -> i64 {
    (days + 3).rem_euclid(7) + 1
}

/// 日期算子支持的时间单位。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DateUnit {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
    Millisecond,
}

impl DateUnit {
    fn parse(value: &str, operator: &str) -> Result<Self, CommandError> {
        Ok(match value {
            "year" => Self::Year,
            "quarter" => Self::Quarter,
            "month" => Self::Month,
            "week" => Self::Week,
            "day" => Self::Day,
            "hour" => Self::Hour,
            "minute" => Self::Minute,
            "second" => Self::Second,
            "millisecond" => Self::Millisecond,
            other => return Err(CommandError::new(2, format!("{operator} does not support unit \"{other}\""))),
        })
    }

    /// 年 / 季 / 月折算的月数；其余单位返回 0。
    fn months(self) -> i64 {
        match self {
            Self::Year => 12,
            Self::Quarter => 3,
            Self::Month => 1,
            _ => 0,
        }
    }
}

/// 解析 `startOfWeek`，返回 Sunday 记 1 的星期序号；缺省为 Sunday。
pub(crate) fn parse_start_of_week(options: &BsonDocument, operator: &str) -> Result<i64, CommandError> {
    let Some(expression) = options.get("startOfWeek")
    else {
        return Ok(1);
    };
    let name = match expression {
        BsonValue::String(name) => name.to_ascii_lowercase(),
        other => return Err(CommandError::new(2, format!("{operator} startOfWeek must be a string, found: {}", bson_type_name(other)))),
    };
    Ok(match name.as_str() {
        "sunday" | "sun" => 1,
        "monday" | "mon" => 2,
        "tuesday" | "tue" => 3,
        "wednesday" | "wed" => 4,
        "thursday" | "thu" => 5,
        "friday" | "fri" => 6,
        "saturday" | "sat" => 7,
        other => return Err(CommandError::new(2, format!("{operator} does not recognize startOfWeek \"{other}\""))),
    })
}

/// 求值日期类聚合算子。
///
/// 语义按 `mongod` 手册实现：`null` 输入返回 `null`，字段算子接受 Date、
/// Timestamp 与 ObjectId。时区只支持 UTC 与固定偏移，`$dateFromString` 的格式
/// 串只支持本文档列出的数字型占位符。所有错误码、错误措辞、月末截断、周边界与
/// `$dateTrunc` 的参考日期语义均待真实 `mongod 8.0.9` 录制核对。
pub(crate) fn evaluate_date_operator(
    document: &BsonDocument,
    operator: &str,
    argument: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<BsonValue, CommandError> {
    match operator {
        "$year" | "$month" | "$dayOfMonth" | "$hour" | "$minute" | "$second" | "$millisecond" | "$dayOfWeek" | "$dayOfYear" | "$week"
        | "$isoWeek" | "$isoWeekYear" | "$isoDayOfWeek" => {
            let Some(value) = evaluate_date_argument(document, argument, variables, operator)?
            else {
                return Ok(BsonValue::Null);
            };
            let civil = CivilDateTime::from_millis(local_millis(value)?);
            let number = match operator {
                "$year" => civil.year,
                "$month" => i64::from(civil.month),
                "$dayOfMonth" => i64::from(civil.day),
                "$hour" => i64::from(civil.hour),
                "$minute" => i64::from(civil.minute),
                "$second" => i64::from(civil.second),
                "$millisecond" => i64::from(civil.millisecond),
                "$dayOfWeek" => civil.day_of_week(),
                "$dayOfYear" => civil.day_of_year(),
                "$week" => civil.week_of_year(),
                "$isoWeek" => civil.iso_week().1,
                "$isoWeekYear" => civil.iso_week().0,
                _ => civil.iso_day_of_week(),
            };
            Ok(count_value(number))
        }
        "$dateToString" => {
            let BsonValue::Document(options) = argument
            else {
                return Err(CommandError::new(2, "$dateToString requires a document argument"));
            };
            let date_expression = options.get("date").ok_or_else(|| CommandError::new(2, "$dateToString requires a 'date' field"))?;
            let value = evaluate_expression_vars(document, date_expression, variables)?;
            let Some(millis) = date_millis(&value, operator)?
            else {
                return match options.get("onNull") {
                    Some(expression) => evaluate_expression_vars(document, expression, variables),
                    None => Ok(BsonValue::Null),
                };
            };
            let offset = evaluate_timezone(document, options, variables)?;
            let format = match options.get("format") {
                Some(expression) => match evaluate_expression_vars(document, expression, variables)? {
                    BsonValue::String(format) => format,
                    other => {
                        return Err(CommandError::new(
                            18533,
                            format!("$dateToString format must be a string, found: {}", bson_type_name(&other)),
                        ));
                    }
                },
                None => "%Y-%m-%dT%H:%M:%S.%LZ".to_string(),
            };
            let civil = CivilDateTime::from_millis(local_millis(DateValue { millis, offset })?);
            Ok(BsonValue::String(format_date(&civil, &format, offset)?))
        }
        "$dateToParts" => {
            let BsonValue::Document(options) = argument
            else {
                return Err(CommandError::new(2, "$dateToParts requires a document argument"));
            };
            let date_expression = options.get("date").ok_or_else(|| CommandError::new(2, "$dateToParts requires a 'date' field"))?;
            let value = evaluate_expression_vars(document, date_expression, variables)?;
            let Some(millis) = date_millis(&value, operator)?
            else {
                return Ok(BsonValue::Null);
            };
            let offset = evaluate_timezone(document, options, variables)?;
            let iso = match options.get("iso8601") {
                Some(expression) => match evaluate_expression_vars(document, expression, variables)? {
                    BsonValue::Bool(flag) => flag,
                    other => {
                        return Err(CommandError::new(2, format!("$dateToParts iso8601 must be a boolean, found: {}", bson_type_name(&other))));
                    }
                },
                None => false,
            };
            let civil = CivilDateTime::from_millis(local_millis(DateValue { millis, offset })?);
            let mut parts = BsonDocument::new();
            if iso {
                let (year, week) = civil.iso_week();
                parts.push("isoWeekYear", count_value(year));
                parts.push("isoWeek", count_value(week));
                parts.push("isoDayOfWeek", count_value(civil.iso_day_of_week()));
            }
            else {
                parts.push("year", count_value(civil.year));
                parts.push("month", count_value(i64::from(civil.month)));
                parts.push("day", count_value(i64::from(civil.day)));
            }
            parts.push("hour", count_value(i64::from(civil.hour)));
            parts.push("minute", count_value(i64::from(civil.minute)));
            parts.push("second", count_value(i64::from(civil.second)));
            parts.push("millisecond", count_value(i64::from(civil.millisecond)));
            Ok(BsonValue::Document(parts))
        }
        "$dateFromParts" => {
            let BsonValue::Document(options) = argument
            else {
                return Err(CommandError::new(2, "$dateFromParts requires a document argument"));
            };
            let offset = evaluate_timezone(document, options, variables)?;
            let number = |name: &str| -> Result<Option<i64>, CommandError> {
                match options.get(name) {
                    Some(expression) => match integral_i64(&evaluate_expression_vars(document, expression, variables)?) {
                        Some(number) => Ok(Some(number)),
                        None => Err(CommandError::new(40515, format!("$dateFromParts '{name}' must be an integer"))),
                    },
                    None => Ok(None),
                }
            };
            let hour = number("hour")?.unwrap_or(0);
            let minute = number("minute")?.unwrap_or(0);
            let second = number("second")?.unwrap_or(0);
            let millisecond = number("millisecond")?.unwrap_or(0);
            let time = hour
                .checked_mul(3_600_000)
                .and_then(|value| value.checked_add(minute.checked_mul(60_000)?))
                .and_then(|value| value.checked_add(second.checked_mul(1_000)?))
                .and_then(|value| value.checked_add(millisecond))
                .ok_or_else(|| CommandError::new(2, "$dateFromParts time components overflow"))?;
            let days = if options.get("isoWeekYear").is_some() {
                let year = number("isoWeekYear")?.ok_or_else(|| CommandError::new(2, "$dateFromParts requires 'isoWeekYear'"))?;
                let week = number("isoWeek")?.unwrap_or(1);
                let weekday = number("isoDayOfWeek")?.unwrap_or(1);
                let january_fourth = days_from_civil(year, 1, 4);
                let monday = january_fourth - (iso_day_of_week(january_fourth) - 1);
                week.checked_sub(1)
                    .and_then(|week| week.checked_mul(7))
                    .and_then(|offset| monday.checked_add(offset))
                    .and_then(|offset| offset.checked_add(weekday.checked_sub(1)?))
                    .ok_or_else(|| CommandError::new(2, "$dateFromParts result overflows"))?
            }
            else {
                let year = number("year")?.ok_or_else(|| CommandError::new(2, "$dateFromParts requires a 'year' or 'isoWeekYear' field"))?;
                let month = number("month")?.unwrap_or(1);
                let day = number("day")?.unwrap_or(1);
                let total_months = year
                    .checked_mul(12)
                    .and_then(|value| value.checked_add(month.checked_sub(1)?))
                    .ok_or_else(|| CommandError::new(2, "$dateFromParts month index overflows"))?;
                let normalized_year = total_months.div_euclid(12);
                let normalized_month = (total_months.rem_euclid(12) + 1) as u32;
                date_days(normalized_year, normalized_month, 1)
                    .checked_add(day.checked_sub(1).ok_or_else(|| CommandError::new(2, "$dateFromParts day index overflows"))?)
                    .ok_or_else(|| CommandError::new(2, "$dateFromParts result overflows"))?
            };
            let millis = days
                .checked_mul(86_400_000)
                .and_then(|value| value.checked_add(time))
                .and_then(|value| value.checked_sub(offset.seconds.checked_mul(1000)?))
                .ok_or_else(|| CommandError::new(2, "$dateFromParts result overflows a 64-bit millisecond count"))?;
            Ok(BsonValue::DateTime(millis))
        }
        "$dateAdd" | "$dateSubtract" => {
            let BsonValue::Document(options) = argument
            else {
                return Err(CommandError::new(2, format!("{operator} requires a document argument")));
            };
            let start_expression =
                options.get("startDate").ok_or_else(|| CommandError::new(2, format!("{operator} requires a 'startDate' field")))?;
            let unit_expression = options.get("unit").ok_or_else(|| CommandError::new(2, format!("{operator} requires a 'unit' field")))?;
            let amount_expression =
                options.get("amount").ok_or_else(|| CommandError::new(2, format!("{operator} requires an 'amount' field")))?;
            let offset = evaluate_timezone(document, options, variables)?;
            let value = evaluate_expression_vars(document, start_expression, variables)?;
            let Some(millis) = date_millis(&value, operator)?
            else {
                return Ok(BsonValue::Null);
            };
            let unit = match evaluate_expression_vars(document, unit_expression, variables)? {
                BsonValue::String(name) => DateUnit::parse(&name, operator)?,
                other => return Err(CommandError::new(2, format!("{operator} unit must be a string, found: {}", bson_type_name(&other)))),
            };
            let amount_value = evaluate_expression_vars(document, amount_expression, variables)?;
            let amount = integral_i64(&amount_value).ok_or_else(|| CommandError::new(2, format!("{operator} amount must be an integer")))?;
            let amount = if operator == "$dateSubtract" {
                amount.checked_neg().ok_or_else(|| CommandError::new(2, format!("{operator} amount overflows")))?
            }
            else {
                amount
            };
            Ok(BsonValue::DateTime(add_date(DateValue { millis, offset }, unit, amount)?))
        }
        "$dateDiff" => {
            let BsonValue::Document(options) = argument
            else {
                return Err(CommandError::new(2, "$dateDiff requires a document argument"));
            };
            let start_expression = options.get("startDate").ok_or_else(|| CommandError::new(2, "$dateDiff requires a 'startDate' field"))?;
            let end_expression = options.get("endDate").ok_or_else(|| CommandError::new(2, "$dateDiff requires an 'endDate' field"))?;
            let unit_expression = options.get("unit").ok_or_else(|| CommandError::new(2, "$dateDiff requires a 'unit' field"))?;
            let offset = evaluate_timezone(document, options, variables)?;
            let start_value = evaluate_expression_vars(document, start_expression, variables)?;
            let end_value = evaluate_expression_vars(document, end_expression, variables)?;
            let (Some(start), Some(end)) = (date_millis(&start_value, operator)?, date_millis(&end_value, operator)?)
            else {
                return Ok(BsonValue::Null);
            };
            let unit = match evaluate_expression_vars(document, unit_expression, variables)? {
                BsonValue::String(name) => DateUnit::parse(&name, operator)?,
                other => return Err(CommandError::new(2, format!("$dateDiff unit must be a string, found: {}", bson_type_name(&other)))),
            };
            let start_of_week = parse_start_of_week(options, operator)?;
            let start_local = local_millis(DateValue { millis: start, offset })?;
            let end_local = local_millis(DateValue { millis: end, offset })?;
            let start_civil = CivilDateTime::from_millis(start_local);
            let end_civil = CivilDateTime::from_millis(end_local);
            let difference = match unit {
                DateUnit::Year => end_civil.year - start_civil.year,
                DateUnit::Quarter => {
                    (end_civil.year * 4 + i64::from((end_civil.month - 1) / 3))
                        - (start_civil.year * 4 + i64::from((start_civil.month - 1) / 3))
                }
                DateUnit::Month => {
                    (end_civil.year * 12 + i64::from(end_civil.month) - 1) - (start_civil.year * 12 + i64::from(start_civil.month) - 1)
                }
                DateUnit::Week => week_index(end_civil.days(), start_of_week) - week_index(start_civil.days(), start_of_week),
                DateUnit::Day => end_civil.days() - start_civil.days(),
                DateUnit::Hour => end_local.div_euclid(3_600_000) - start_local.div_euclid(3_600_000),
                DateUnit::Minute => end_local.div_euclid(60_000) - start_local.div_euclid(60_000),
                DateUnit::Second => end_local.div_euclid(1_000) - start_local.div_euclid(1_000),
                DateUnit::Millisecond => end_local - start_local,
            };
            Ok(count_value(difference))
        }
        "$dateTrunc" => {
            let BsonValue::Document(options) = argument
            else {
                return Err(CommandError::new(2, "$dateTrunc requires a document argument"));
            };
            let date_expression = options.get("date").ok_or_else(|| CommandError::new(2, "$dateTrunc requires a 'date' field"))?;
            let unit_expression = options.get("unit").ok_or_else(|| CommandError::new(2, "$dateTrunc requires a 'unit' field"))?;
            let offset = evaluate_timezone(document, options, variables)?;
            let start_of_week = parse_start_of_week(options, operator)?;
            let bin_size = match options.get("binSize") {
                Some(expression) => match integral_i64(&evaluate_expression_vars(document, expression, variables)?) {
                    Some(number) if number > 0 => number,
                    _ => return Err(CommandError::new(2, "$dateTrunc binSize must be a positive integer")),
                },
                None => 1,
            };
            let value = evaluate_expression_vars(document, date_expression, variables)?;
            let Some(millis) = date_millis(&value, operator)?
            else {
                return Ok(BsonValue::Null);
            };
            let unit = match evaluate_expression_vars(document, unit_expression, variables)? {
                BsonValue::String(name) => DateUnit::parse(&name, operator)?,
                other => return Err(CommandError::new(2, format!("$dateTrunc unit must be a string, found: {}", bson_type_name(&other)))),
            };
            let local = local_millis(DateValue { millis, offset })?;
            let truncated = truncate_local(local, unit, bin_size, start_of_week)?;
            Ok(BsonValue::DateTime(
                truncated
                    .checked_sub(offset.seconds.checked_mul(1000).ok_or_else(|| CommandError::new(2, "timezone offset overflows"))?)
                    .ok_or_else(|| CommandError::new(2, "date arithmetic overflows a 64-bit millisecond count"))?,
            ))
        }
        "$dateFromString" => {
            let BsonValue::Document(options) = argument
            else {
                return Err(CommandError::new(2, "$dateFromString requires a document argument"));
            };
            let string_expression =
                options.get("dateString").ok_or_else(|| CommandError::new(2, "$dateFromString requires a 'dateString' field"))?;
            let value = evaluate_expression_vars(document, string_expression, variables)?;
            let text = match value {
                BsonValue::Null | BsonValue::Undefined => {
                    return match options.get("onNull") {
                        Some(expression) => evaluate_expression_vars(document, expression, variables),
                        None => Ok(BsonValue::Null),
                    };
                }
                BsonValue::String(text) => text,
                other => {
                    let message = format!("$dateFromString requires a string, found: {}", bson_type_name(&other));
                    return match options.get("onError") {
                        Some(expression) => evaluate_expression_vars(document, expression, variables),
                        None => Err(CommandError::new(40516, message)),
                    };
                }
            };
            let offset = evaluate_timezone(document, options, variables)?;
            let parsed = match options.get("format") {
                Some(expression) => match evaluate_expression_vars(document, expression, variables)? {
                    BsonValue::String(format) => parse_formatted_datetime(&text, &format),
                    other => Err(format!("format must be a string, found: {}", bson_type_name(&other))),
                },
                None => parse_iso_datetime(&text),
            };
            match parsed {
                Ok((local, embedded)) => {
                    let shift = embedded.unwrap_or(offset.seconds);
                    let millis = local
                        .checked_sub(shift.checked_mul(1000).ok_or_else(|| CommandError::new(2, "timezone offset overflows"))?)
                        .ok_or_else(|| CommandError::new(2, "date arithmetic overflows a 64-bit millisecond count"))?;
                    Ok(BsonValue::DateTime(millis))
                }
                Err(message) => match options.get("onError") {
                    Some(expression) => evaluate_expression_vars(document, expression, variables),
                    None => Err(CommandError::new(40516, format!("$dateFromString could not parse \"{text}\": {message}"))),
                },
            }
        }
        other => unreachable!("evaluate_date_operator called with {other}"),
    }
}

/// 转换类算子的目标 BSON 类型。
///
/// 只覆盖 `$convert` 手册列出的标量目标；`binData` 需要子类型语义，当前
/// 直接拒绝，缺口在待真机清单里跟踪。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConversionTarget {
    Bool,
    Date,
    Decimal,
    Double,
    Int,
    Long,
    ObjectId,
    String,
}

impl ConversionTarget {
    /// 解析 `$convert` 的 `to` 类型别名。
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "bool" => Self::Bool,
            "date" => Self::Date,
            "decimal" => Self::Decimal,
            "double" => Self::Double,
            "int" => Self::Int,
            "long" => Self::Long,
            "objectId" => Self::ObjectId,
            "string" => Self::String,
            _ => return None,
        })
    }

    /// 解析 `$convert` 的 `to` 数值类型码，沿用 BSON 元素编号。
    fn from_code(code: i64) -> Option<Self> {
        Some(match code {
            1 => Self::Double,
            2 => Self::String,
            7 => Self::ObjectId,
            8 => Self::Bool,
            9 => Self::Date,
            16 => Self::Int,
            18 => Self::Long,
            19 => Self::Decimal,
            _ => return None,
        })
    }

    /// 错误消息与 `$type` 输出共用的类型名。
    fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Date => "date",
            Self::Decimal => "decimal",
            Self::Double => "double",
            Self::Int => "int",
            Self::Long => "long",
            Self::ObjectId => "objectId",
            Self::String => "string",
        }
    }
}

/// 求值转换类聚合算子。
///
/// `$toX` 是 `$convert` 的语法糖：`null` 与缺失返回 `null`，转换失败抛
/// `ConversionFailure`（`241`）。`$convert` 额外支持 `onNull` / `onError`。
/// `$type` 对缺失字段返回 `"missing"`，而当前表达式求值把缺失折叠成 `null`，
/// 因此这里单独回查字段路径。精确错误码、错误措辞、`double` 的科学计数写法与
/// 边界值处理均待真实 `mongod 8.0.9` 录制核对。
pub(crate) fn evaluate_conversion_operator(
    document: &BsonDocument,
    operator: &str,
    argument: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<BsonValue, CommandError> {
    match operator {
        "$isNumber" => {
            let value = evaluate_expression_vars(document, argument, variables)?;
            Ok(BsonValue::Bool(matches!(value, BsonValue::Int32(_) | BsonValue::Int64(_) | BsonValue::Double(_) | BsonValue::Decimal128(_))))
        }
        "$type" => {
            let value = evaluate_expression_vars(document, argument, variables)?;
            // 缺失字段与显式 `undefined` 都按 `"missing"` 上报。
            let missing = matches!(value, BsonValue::Undefined)
                || matches!(argument, BsonValue::String(path)
                    if path.starts_with('$') && !path.starts_with("$$") && lookup_field(document, &path[1..]).is_none());
            Ok(BsonValue::String(if missing { "missing".to_string() } else { bson_type_name(&value).to_string() }))
        }
        "$convert" => evaluate_convert(document, argument, variables),
        _ => {
            let target = match operator {
                "$toBool" => ConversionTarget::Bool,
                "$toDate" => ConversionTarget::Date,
                "$toDecimal" => ConversionTarget::Decimal,
                "$toDouble" => ConversionTarget::Double,
                "$toInt" => ConversionTarget::Int,
                "$toLong" => ConversionTarget::Long,
                "$toObjectId" => ConversionTarget::ObjectId,
                _ => ConversionTarget::String,
            };
            let value = evaluate_expression_vars(document, argument, variables)?;
            if matches!(value, BsonValue::Null | BsonValue::Undefined) {
                return Ok(BsonValue::Null);
            }
            convert_value(&value, target)
        }
    }
}

/// 求值 `$convert: { input, to, onNull, onError }`。
pub(crate) fn evaluate_convert(
    document: &BsonDocument,
    argument: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<BsonValue, CommandError> {
    let BsonValue::Document(options) = argument
    else {
        return Err(CommandError::new(2, "$convert requires an object argument"));
    };
    let input_expression = options.get("input").ok_or_else(|| CommandError::new(2, "$convert requires an 'input' field"))?;
    let to_expression = options.get("to").ok_or_else(|| CommandError::new(2, "$convert requires a 'to' field"))?;
    let target = match evaluate_expression_vars(document, to_expression, variables)? {
        BsonValue::String(name) => {
            ConversionTarget::parse(&name).ok_or_else(|| CommandError::new(2, format!("Unknown type name alias: {name}")))?
        }
        BsonValue::Document(_) => {
            return Err(CommandError::new(2, "conversion to binData is not implemented"));
        }
        other => match integral_i64(&other) {
            Some(code) => {
                ConversionTarget::from_code(code).ok_or_else(|| CommandError::new(2, format!("Invalid numerical type code: {code}")))?
            }
            None => {
                return Err(CommandError::new(
                    2,
                    format!("$convert 'to' must be a type name or a numeric type code, found: {}", bson_type_name(&other)),
                ));
            }
        },
    };
    let value = evaluate_expression_vars(document, input_expression, variables)?;
    if matches!(value, BsonValue::Null | BsonValue::Undefined) {
        return match options.get("onNull") {
            Some(expression) => evaluate_expression_vars(document, expression, variables),
            None => Ok(BsonValue::Null),
        };
    }
    match convert_value(&value, target) {
        Ok(result) => Ok(result),
        Err(error) => match options.get("onError") {
            Some(expression) => evaluate_expression_vars(document, expression, variables),
            None => Err(error),
        },
    }
}

/// 按目标类型转换一个非空值。
pub(crate) fn convert_value(value: &BsonValue, target: ConversionTarget) -> Result<BsonValue, CommandError> {
    match target {
        ConversionTarget::Bool => convert_to_bool(value),
        ConversionTarget::Date => convert_to_date(value),
        ConversionTarget::Decimal => convert_to_decimal(value),
        ConversionTarget::Double => convert_to_double(value),
        ConversionTarget::Int => convert_to_integer(value, ConversionTarget::Int),
        ConversionTarget::Long => convert_to_integer(value, ConversionTarget::Long),
        ConversionTarget::ObjectId => convert_to_object_id(value),
        ConversionTarget::String => convert_to_string(value),
    }
}

pub(crate) fn conversion_failure(value: &BsonValue, target: ConversionTarget) -> CommandError {
    CommandError::new(
        241,
        format!("Unsupported conversion from {} to {} in $convert with no onError value", bson_type_name(value), target.name()),
    )
}

pub(crate) fn convert_to_bool(value: &BsonValue) -> Result<BsonValue, CommandError> {
    let flag = match value {
        BsonValue::Bool(flag) => *flag,
        BsonValue::Int32(number) => *number != 0,
        BsonValue::Int64(number) => *number != 0,
        BsonValue::Double(number) => *number != 0.0,
        BsonValue::Decimal128(decimal) => decimal.as_f64() != 0.0,
        // 任何字符串（含空串）、日期与 ObjectId 都转成 `true`。
        BsonValue::String(_) | BsonValue::DateTime(_) | BsonValue::ObjectId(_) => true,
        other => return Err(conversion_failure(other, ConversionTarget::Bool)),
    };
    Ok(BsonValue::Bool(flag))
}

pub(crate) fn convert_to_date(value: &BsonValue) -> Result<BsonValue, CommandError> {
    let millis = match value {
        BsonValue::DateTime(millis) => *millis,
        BsonValue::Timestamp { t, .. } => i64::from(*t) * 1000,
        BsonValue::ObjectId(bytes) => i64::from(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])) * 1000,
        BsonValue::Int32(number) => i64::from(*number),
        BsonValue::Int64(number) => *number,
        BsonValue::Double(number) => truncated_double(*number).ok_or_else(|| conversion_failure(value, ConversionTarget::Date))?,
        BsonValue::Decimal128(decimal) => decimal.as_i64().ok_or_else(|| conversion_failure(value, ConversionTarget::Date))?,
        BsonValue::String(text) => {
            let (local, embedded) = parse_iso_datetime(text).map_err(|_| conversion_failure(value, ConversionTarget::Date))?;
            local
                .checked_sub(embedded.unwrap_or(0).checked_mul(1000).ok_or_else(|| conversion_failure(value, ConversionTarget::Date))?)
                .ok_or_else(|| conversion_failure(value, ConversionTarget::Date))?
        }
        other => return Err(conversion_failure(other, ConversionTarget::Date)),
    };
    Ok(BsonValue::DateTime(millis))
}

pub(crate) fn convert_to_decimal(value: &BsonValue) -> Result<BsonValue, CommandError> {
    let text = match value {
        BsonValue::Bool(flag) => if *flag { "1" } else { "0" }.to_string(),
        BsonValue::Int32(number) => number.to_string(),
        BsonValue::Int64(number) => number.to_string(),
        BsonValue::Double(number) => display_double(*number),
        BsonValue::Decimal128(decimal) => return Ok(BsonValue::Decimal128(*decimal)),
        BsonValue::String(text) => text.clone(),
        BsonValue::DateTime(millis) => millis.to_string(),
        other => return Err(conversion_failure(other, ConversionTarget::Decimal)),
    };
    Decimal128::from_str(&text).map(BsonValue::Decimal128).ok_or_else(|| conversion_failure(value, ConversionTarget::Decimal))
}

pub(crate) fn convert_to_double(value: &BsonValue) -> Result<BsonValue, CommandError> {
    let number = match value {
        BsonValue::Bool(flag) => {
            if *flag {
                1.0
            }
            else {
                0.0
            }
        }
        BsonValue::Int32(number) => f64::from(*number),
        BsonValue::Int64(number) => *number as f64,
        BsonValue::Double(number) => *number,
        BsonValue::Decimal128(decimal) => decimal.as_f64(),
        BsonValue::String(text) => text.parse::<f64>().map_err(|_| conversion_failure(value, ConversionTarget::Double))?,
        BsonValue::DateTime(millis) => *millis as f64,
        other => return Err(conversion_failure(other, ConversionTarget::Double)),
    };
    Ok(BsonValue::Double(number))
}

pub(crate) fn convert_to_integer(value: &BsonValue, target: ConversionTarget) -> Result<BsonValue, CommandError> {
    let number = match value {
        BsonValue::Bool(flag) => i64::from(*flag),
        BsonValue::Int32(number) => i64::from(*number),
        BsonValue::Int64(number) => *number,
        BsonValue::Double(number) => truncated_double(*number).ok_or_else(|| conversion_failure(value, target))?,
        BsonValue::Decimal128(decimal) => {
            decimal.as_i64().or_else(|| truncated_double(decimal.as_f64())).ok_or_else(|| conversion_failure(value, target))?
        }
        BsonValue::String(text) => text.parse::<i64>().map_err(|_| conversion_failure(value, target))?,
        BsonValue::DateTime(millis) => *millis,
        other => return Err(conversion_failure(other, target)),
    };
    match target {
        ConversionTarget::Int => i32::try_from(number).map(BsonValue::Int32).map_err(|_| conversion_failure(value, target)),
        _ => Ok(BsonValue::Int64(number)),
    }
}

pub(crate) fn convert_to_object_id(value: &BsonValue) -> Result<BsonValue, CommandError> {
    match value {
        BsonValue::ObjectId(bytes) => Ok(BsonValue::ObjectId(*bytes)),
        BsonValue::String(text) if text.len() == 24 => {
            let mut bytes = [0_u8; 12];
            for (index, slot) in bytes.iter_mut().enumerate() {
                let pair = &text[index * 2..index * 2 + 2];
                *slot = u8::from_str_radix(pair, 16).map_err(|_| conversion_failure(value, ConversionTarget::ObjectId))?;
            }
            Ok(BsonValue::ObjectId(bytes))
        }
        other => Err(conversion_failure(other, ConversionTarget::ObjectId)),
    }
}

pub(crate) fn convert_to_string(value: &BsonValue) -> Result<BsonValue, CommandError> {
    let text = match value {
        BsonValue::String(text) => text.clone(),
        BsonValue::Bool(flag) => if *flag { "true" } else { "false" }.to_string(),
        BsonValue::Int32(number) => number.to_string(),
        BsonValue::Int64(number) => number.to_string(),
        BsonValue::Double(number) => display_double(*number),
        BsonValue::Decimal128(decimal) => decimal.as_str(),
        BsonValue::ObjectId(bytes) => hex_text(bytes),
        BsonValue::DateTime(millis) => format_date(&CivilDateTime::from_millis(*millis), "%Y-%m-%dT%H:%M:%S.%LZ", FixedOffset::utc())?,
        other => return Err(conversion_failure(other, ConversionTarget::String)),
    };
    Ok(BsonValue::String(text))
}

/// 按 `mongod` 的 `Double.toString()` 口径渲染双精度值。
///
/// 非有限值写成 `"NaN"` / `"Infinity"`；有限值交给 Rust 的最短往返表示，
/// 其科学计数写法（`1e300` 与 `1E+300`）待真机核对。
pub(crate) fn display_double(number: f64) -> String {
    if number.is_nan() {
        "NaN".to_string()
    }
    else if number.is_infinite() {
        if number.is_sign_negative() { "-Infinity".to_string() } else { "Infinity".to_string() }
    }
    else {
        number.to_string()
    }
}

/// 双精度值截断到整数部分，超出 `i64` 或非有限值返回 `None`。
pub(crate) fn truncated_double(number: f64) -> Option<i64> {
    if !number.is_finite() {
        return None;
    }
    let truncated = number.trunc();
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    if truncated >= -TWO_POW_63 && truncated < TWO_POW_63 { Some(truncated as i64) } else { None }
}

/// 按 `$dateToString` 的格式串渲染日期。
///
/// 支持的占位符：`%Y %m %d %H %M %S %L %j %w %u %U %V %G %z %Z %%`。其余
/// 占位符按不支持处理，具体错误码与措辞待真机核对。
pub(crate) fn format_date(civil: &CivilDateTime, format: &str, offset: FixedOffset) -> Result<String, CommandError> {
    let mut output = String::new();
    let mut characters = format.chars();
    while let Some(character) = characters.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        let specifier = characters.next().ok_or_else(|| CommandError::new(18536, "$dateToString format ends with a lone '%'"))?;
        match specifier {
            'Y' => output.push_str(&format!("{:04}", civil.year)),
            'm' => output.push_str(&format!("{:02}", civil.month)),
            'd' => output.push_str(&format!("{:02}", civil.day)),
            'H' => output.push_str(&format!("{:02}", civil.hour)),
            'M' => output.push_str(&format!("{:02}", civil.minute)),
            'S' => output.push_str(&format!("{:02}", civil.second)),
            'L' => output.push_str(&format!("{:03}", civil.millisecond)),
            'j' => output.push_str(&format!("{:03}", civil.day_of_year())),
            'w' => output.push_str(&format!("{:01}", civil.day_of_week())),
            'u' => output.push_str(&format!("{:01}", civil.iso_day_of_week())),
            'U' => output.push_str(&format!("{:02}", civil.week_of_year())),
            'V' => output.push_str(&format!("{:02}", civil.iso_week().1)),
            'G' => output.push_str(&format!("{:04}", civil.iso_week().0)),
            'z' => output.push_str(&offset.text()),
            'Z' => output.push_str(&offset.seconds.div_euclid(60).to_string()),
            '%' => output.push('%'),
            other => return Err(CommandError::new(18536, format!("unrecognized format specifier: %{other}"))),
        }
    }
    Ok(output)
}

/// 解析 ISO-8601 日期时间，返回本地毫秒与串内自带的偏移秒数。
///
/// 支持 `YYYY-MM-DD`、可选的 `THH:MM[:SS[.fff]]` 与可选的 `Z` / `±HH[:MM]`。
/// 闰秒、负年份与扩展年份不支持；具体报错文案待真机核对。
pub(crate) fn parse_iso_datetime(text: &str) -> Result<(i64, Option<i64>), String> {
    let text = text.trim();
    let separator = text.find(['T', 't', ' ']);
    let (date_part, time_part) = match separator {
        Some(index) => (&text[..index], Some(&text[index + 1..])),
        None => (text, None),
    };
    if date_part.len() != 10 || !date_part.is_ascii() {
        return Err(format!("cannot parse \"{text}\" as an ISO-8601 date"));
    }
    let bytes = date_part.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(format!("cannot parse \"{text}\" as an ISO-8601 date"));
    }
    let year: i64 = date_part[0..4].parse().map_err(|_| format!("cannot parse year in \"{text}\""))?;
    let month: u32 = date_part[5..7].parse().map_err(|_| format!("cannot parse month in \"{text}\""))?;
    let day: u32 = date_part[8..10].parse().map_err(|_| format!("cannot parse day in \"{text}\""))?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, i64::from(month)) as u32 {
        return Err(format!("date out of range in \"{text}\""));
    }
    let (hour, minute, second, millisecond, offset) = match time_part {
        Some(rest) => parse_clock_with_zone(rest, text)?,
        None => (0, 0, 0, 0, None),
    };
    let local = CivilDateTime { year, month, day, hour, minute, second, millisecond }.to_millis();
    Ok((local, offset))
}

/// 解析 `HH:MM[:SS[.fff]]` 与其后的时区偏移。
pub(crate) fn parse_clock_with_zone(text: &str, original: &str) -> Result<(u32, u32, u32, u32, Option<i64>), String> {
    let (clock, offset) = if text.ends_with('Z') || text.ends_with('z') {
        (&text[..text.len() - 1], Some(0))
    }
    else {
        match text[1..].find(['+', '-']).map(|index| index + 1) {
            Some(index) => {
                let offset = FixedOffset::parse(&text[index..]).map_err(|error| error.message)?;
                (&text[..index], Some(offset.seconds))
            }
            None => (text, None),
        }
    };
    let mut parts = clock.split(':');
    let hour: u32 = parts
        .next()
        .ok_or_else(|| format!("missing hour in \"{original}\""))?
        .parse()
        .map_err(|_| format!("cannot parse hour in \"{original}\""))?;
    let minute: u32 = parts
        .next()
        .ok_or_else(|| format!("missing minute in \"{original}\""))?
        .parse()
        .map_err(|_| format!("cannot parse minute in \"{original}\""))?;
    let (second, millisecond) = match parts.next() {
        Some(part) => {
            let (second_text, fraction) = match part.split_once('.') {
                Some((second, fraction)) => (second, Some(fraction)),
                None => (part, None),
            };
            let second: u32 = second_text.parse().map_err(|_| format!("cannot parse second in \"{original}\""))?;
            let millisecond = match fraction {
                Some(fraction) => {
                    if fraction.is_empty() || fraction.len() > 9 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
                        return Err(format!("cannot parse fractional seconds in \"{original}\""));
                    }
                    let digits: String = format!("{fraction:0<3}").chars().take(3).collect();
                    digits.parse().map_err(|_| format!("cannot parse fractional seconds in \"{original}\""))?
                }
                None => 0,
            };
            (second, millisecond)
        }
        None => (0, 0),
    };
    if parts.next().is_some() {
        return Err(format!("trailing time components in \"{original}\""));
    }
    if hour > 23 || minute > 59 || second > 59 {
        return Err(format!("time out of range in \"{original}\""));
    }
    Ok((hour, minute, second, millisecond, offset))
}

/// 按格式串解析日期时间。
///
/// 只支持 `%Y %m %d %H %M %S %L %z %Z %%`，且数字宽度按输出格式固定；其余
/// 占位符与宽松匹配行为待真机核对。
pub(crate) fn parse_formatted_datetime(text: &str, format: &str) -> Result<(i64, Option<i64>), String> {
    let mut year = 1970i64;
    let mut month = 1u32;
    let mut day = 1u32;
    let mut hour = 0u32;
    let mut minute = 0u32;
    let mut second = 0u32;
    let mut millisecond = 0u32;
    let mut offset = None;
    let mut remaining = text;
    let mut characters = format.chars();
    while let Some(character) = characters.next() {
        if character != '%' {
            let mut buffer = [0u8; 4];
            let literal: &str = character.encode_utf8(&mut buffer);
            remaining = remaining.strip_prefix(literal).ok_or_else(|| format!("expected '{character}' in \"{text}\""))?;
            continue;
        }
        let specifier = characters.next().ok_or_else(|| "format ends with a lone '%'".to_string())?;
        match specifier {
            '%' => remaining = remaining.strip_prefix('%').ok_or_else(|| format!("expected '%' in \"{text}\""))?,
            'z' | 'Z' => {
                let (zone, rest) = parse_zone_prefix(remaining)?;
                offset = Some(zone);
                remaining = rest;
            }
            'Y' | 'm' | 'd' | 'H' | 'M' | 'S' | 'L' => {
                let width = if specifier == 'Y' {
                    4
                }
                else if specifier == 'L' {
                    3
                }
                else {
                    2
                };
                let (digits, rest) = take_digits(remaining, width)?;
                let number: u32 = digits.parse().map_err(|_| format!("cannot parse %{specifier} in \"{text}\""))?;
                match specifier {
                    'Y' => year = i64::from(number),
                    'm' => month = number,
                    'd' => day = number,
                    'H' => hour = number,
                    'M' => minute = number,
                    'S' => second = number,
                    _ => millisecond = number,
                }
                remaining = rest;
            }
            other => return Err(format!("unrecognized format specifier: %{other}")),
        }
    }
    if !remaining.is_empty() {
        return Err(format!("trailing characters in \"{text}\""));
    }
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, i64::from(month)) as u32 {
        return Err(format!("date out of range in \"{text}\""));
    }
    if hour > 23 || minute > 59 || second > 59 {
        return Err(format!("time out of range in \"{text}\""));
    }
    Ok((CivilDateTime { year, month, day, hour, minute, second, millisecond }.to_millis(), offset))
}

/// 从输入开头取固定宽度的 ASCII 数字。
pub(crate) fn take_digits(text: &str, width: usize) -> Result<(&str, &str), String> {
    if text.len() < width || !text.as_bytes()[..width].iter().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("expected {width} digits"));
    }
    Ok((&text[..width], &text[width..]))
}

/// 从输入开头取时区偏移，返回偏移秒数与剩余文本。
pub(crate) fn parse_zone_prefix(text: &str) -> Result<(i64, &str), String> {
    let bytes = text.as_bytes();
    match bytes.first() {
        Some(b'Z') | Some(b'z') => return Ok((0, &text[1..])),
        Some(b'+') | Some(b'-') => {}
        _ => return Err("missing timezone offset".to_string()),
    }
    let length = if text.len() >= 6 && bytes[3] == b':' {
        6
    }
    else if text.len() >= 5 {
        5
    }
    else if text.len() >= 3 {
        3
    }
    else {
        return Err("truncated timezone offset".to_string());
    };
    let offset = FixedOffset::parse(&text[..length]).map_err(|error| error.message)?;
    Ok((offset.seconds, &text[length..]))
}

/// 按单位平移日期。年 / 季 / 月先落到目标月的同一天，超出月末则截到月末。
pub(crate) fn add_date(value: DateValue, unit: DateUnit, amount: i64) -> Result<i64, CommandError> {
    if unit.months() > 0 {
        let civil = CivilDateTime::from_millis(local_millis(value)?);
        let months =
            amount.checked_mul(unit.months()).ok_or_else(|| CommandError::new(2, "date arithmetic overflows a 64-bit millisecond count"))?;
        let total = civil
            .year
            .checked_mul(12)
            .and_then(|total| total.checked_add(i64::from(civil.month) - 1))
            .and_then(|total| total.checked_add(months))
            .ok_or_else(|| CommandError::new(2, "date arithmetic overflows a 64-bit millisecond count"))?;
        let year = total.div_euclid(12);
        let month = (total.rem_euclid(12) + 1) as u32;
        let day = civil.day.min(days_in_month(year, i64::from(month)) as u32);
        let updated = CivilDateTime { year, month, day, ..civil };
        let offset = value.offset.seconds.checked_mul(1000).ok_or_else(|| CommandError::new(2, "timezone offset overflows"))?;
        return updated
            .to_millis()
            .checked_sub(offset)
            .ok_or_else(|| CommandError::new(2, "date arithmetic overflows a 64-bit millisecond count"));
    }
    let unit_millis = match unit {
        DateUnit::Week => 7 * 86_400_000,
        DateUnit::Day => 86_400_000,
        DateUnit::Hour => 3_600_000,
        DateUnit::Minute => 60_000,
        DateUnit::Second => 1_000,
        _ => 1,
    };
    let delta = amount.checked_mul(unit_millis).ok_or_else(|| CommandError::new(2, "date arithmetic overflows a 64-bit millisecond count"))?;
    value.millis.checked_add(delta).ok_or_else(|| CommandError::new(2, "date arithmetic overflows a 64-bit millisecond count"))
}

/// 以 `startOfWeek` 为界，把本地天数折算成周序号。
pub(crate) fn week_index(days: i64, start_of_week: i64) -> i64 {
    // 1970-01-01 是星期四，Sunday 记 1 时为 5。
    let reference = start_of_week - 5;
    (days - reference).div_euclid(7)
}

/// `$dateTrunc` 的参考点是本地时间 2000-01-01T00:00:00，按 `binSize` 向下取整。
pub(crate) fn truncate_local(local: i64, unit: DateUnit, bin_size: i64, start_of_week: i64) -> Result<i64, CommandError> {
    let reference_days = days_from_civil(2000, 1, 1);
    let reference_millis = reference_days * 86_400_000;
    let zero = |year: i64, month: u32| CivilDateTime { year, month, day: 1, hour: 0, minute: 0, second: 0, millisecond: 0 };
    Ok(match unit {
        DateUnit::Year => {
            let civil = CivilDateTime::from_millis(local);
            let index = (civil.year - 2000).div_euclid(bin_size);
            zero(2000 + index * bin_size, 1).to_millis()
        }
        DateUnit::Quarter => {
            let civil = CivilDateTime::from_millis(local);
            let index = (civil.year - 2000) * 4 + i64::from((civil.month - 1) / 3);
            let start = index.div_euclid(bin_size) * bin_size;
            zero(2000 + start.div_euclid(4), (start.rem_euclid(4) * 3 + 1) as u32).to_millis()
        }
        DateUnit::Month => {
            let civil = CivilDateTime::from_millis(local);
            let index = (civil.year - 2000) * 12 + i64::from(civil.month) - 1;
            let start = index.div_euclid(bin_size) * bin_size;
            zero(2000 + start.div_euclid(12), (start.rem_euclid(12) + 1) as u32).to_millis()
        }
        DateUnit::Week => {
            let days = local.div_euclid(86_400_000);
            let reference_week = reference_days - (sunday_day_of_week(reference_days) - start_of_week).rem_euclid(7);
            let index = (days - reference_week).div_euclid(7 * bin_size);
            (reference_week + index * 7 * bin_size)
                .checked_mul(86_400_000)
                .ok_or_else(|| CommandError::new(2, "$dateTrunc result overflows a 64-bit millisecond count"))?
        }
        DateUnit::Day | DateUnit::Hour | DateUnit::Minute | DateUnit::Second | DateUnit::Millisecond => {
            let unit_millis = match unit {
                DateUnit::Day => 86_400_000,
                DateUnit::Hour => 3_600_000,
                DateUnit::Minute => 60_000,
                DateUnit::Second => 1_000,
                _ => 1,
            };
            let index = (local - reference_millis).div_euclid(unit_millis * bin_size);
            reference_millis
                .checked_add(
                    index
                        .checked_mul(unit_millis * bin_size)
                        .ok_or_else(|| CommandError::new(2, "$dateTrunc result overflows a 64-bit millisecond count"))?,
                )
                .ok_or_else(|| CommandError::new(2, "$dateTrunc result overflows a 64-bit millisecond count"))?
        }
    })
}
