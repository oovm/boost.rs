//! MongoDB 过滤器：操作符校验、归一化与逐文档求值。
//!
//! 校验在读取任何文档之前完成，未知操作符是硬错误而不是「匹配零篇」。求值只读
//! 候选文档；`$expr` 需要的聚合表达式求值由调用方通过 [`ExpressionContext`] 提供。

use std::{cmp::Ordering, collections::BTreeMap};

use database_protocol_mongo::bson::{BsonDocument, BsonValue};

use crate::mongo::{
    CommandError,
    value::{compare_values, expression_truthy, integer_of, integral_i64, number_of, value_rank, values_equal},
};

/// 聚合变量（`$$名称`）。`$lookup` 的 `let` 等阶段写入，过滤器只读。
pub type ExpressionVariables = BTreeMap<String, BsonValue>;

/// `$expr` 谓词的聚合表达式求值端口。
///
/// 表达式求值属于执行语义，实现在 `database.execution::mongo`；过滤器只声明它
/// 需要的这一个动作，避免 query 反向依赖执行层。
pub trait ExpressionContext {
    /// 按当前文档与变量求值一条聚合表达式。
    fn evaluate(&self, document: &BsonDocument, expression: &BsonValue, variables: &ExpressionVariables) -> Result<BsonValue, CommandError>;
}

/// Filter operators this gateway implements. Anything outside this list is a
/// hard error: an unrecognised operator must never silently match nothing.
pub const FILTER_OPERATORS: [&str; 21] = [
    "$eq",
    "$ne",
    "$gt",
    "$gte",
    "$lt",
    "$lte",
    "$in",
    "$nin",
    "$exists",
    "$regex",
    "$options",
    "$size",
    "$type",
    "$elemMatch",
    "$all",
    "$not",
    "$mod",
    "$bitsAllSet",
    "$bitsAnySet",
    "$bitsAllClear",
    "$bitsAnyClear",
];

/// Logical operators accepted at the top of a filter.
pub const LOGICAL_OPERATORS: [&str; 3] = ["$and", "$or", "$nor"];

/// Rejects every filter that uses an operator outside [`FILTER_OPERATORS`].
///
/// The message always names the operator, the operation that received it, and
/// where it appeared (`fieldIndex` within the filter, `operatorIndex` within
/// the operator document), so a wrong query is reported instead of silently
/// returning an empty cursor.
pub fn validate_filter(filter: &BsonDocument, operation: &str) -> Result<(), CommandError> {
    for (field_index, (field, expected)) in filter.fields.iter().enumerate() {
        if field.starts_with('$') {
            if LOGICAL_OPERATORS.contains(&field.as_str()) {
                let BsonValue::Array(clauses) = expected
                else {
                    return Err(CommandError::new(
                        2,
                        format!("{field} requires an array of filter documents (operation: {operation}, fieldIndex: {field_index})"),
                    ));
                };
                if clauses.is_empty() {
                    return Err(CommandError::new(
                        2,
                        format!(
                            "{field} with an empty array is not defined by the MongoDB manual; refusing to guess (operation: {operation}, fieldIndex: {field_index})"
                        ),
                    ));
                }
                for (clause_index, clause) in clauses.iter().enumerate() {
                    let BsonValue::Document(clause) = clause
                    else {
                        return Err(CommandError::new(
                            2,
                            format!("{field} element {clause_index} is not a filter document (operation: {operation})"),
                        ));
                    };
                    validate_filter(clause, operation)?;
                }
                continue;
            }
            if field == "$expr" {
                // `$expr` carries an aggregation expression, validated when it is
                // evaluated against each candidate document.
                continue;
            }
            return Err(CommandError::new(
                2,
                format!("unknown top level operator: {field} (operation: {operation}, fieldIndex: {field_index})"),
            ));
        }
        let BsonValue::Document(operators) = expected
        else {
            continue;
        };
        if !has_operator(operators) {
            continue;
        }
        validate_operator_document(operators, operation, field, field_index)?;
    }
    Ok(())
}

/// A document is an operator document when any field starts with `$`.
pub fn has_operator(document: &BsonDocument) -> bool {
    document.fields.iter().any(|(field, _)| field.starts_with('$'))
}

/// The planner reports every equality as `{$eq: value}` and joins multiple
/// fields with `$and` in field-name order.
pub fn normalize_query(filter: &BsonDocument) -> BsonDocument {
    if filter.fields.is_empty() {
        return BsonDocument::new();
    }
    let mut clauses = Vec::new();
    for (field, expected) in &filter.fields {
        if field.starts_with('$') {
            let mut clause = BsonDocument::new();
            clause.push(field.clone(), expected.clone());
            clauses.push((field.clone(), clause));
            continue;
        }
        let mut clause = BsonDocument::new();
        let normalized = match expected {
            BsonValue::Document(operators) if has_operator(operators) => expected.clone(),
            other => {
                let mut equality = BsonDocument::new();
                equality.push("$eq".to_string(), other.clone());
                BsonValue::Document(equality)
            }
        };
        clause.push(field.clone(), normalized);
        clauses.push((field.clone(), clause));
    }
    if clauses.len() == 1 {
        return clauses.pop().unwrap().1;
    }
    clauses.sort_by(|left, right| left.0.cmp(&right.0));
    let mut combined = BsonDocument::new();
    combined.push("$and".to_string(), BsonValue::Array(clauses.into_iter().map(|(_, clause)| BsonValue::Document(clause)).collect()));
    combined
}

pub(crate) fn validate_operator_document(
    operators: &BsonDocument,
    operation: &str,
    field: &str,
    field_index: usize,
) -> Result<(), CommandError> {
    for (operator_index, (operator, operand)) in operators.fields.iter().enumerate() {
        if !FILTER_OPERATORS.contains(&operator.as_str()) {
            return Err(unknown_operator(operator, operation, field, field_index, operator_index));
        }
        let bad = |detail: &str| {
            CommandError::new(
                2,
                format!(
                    "{operator} {detail} (operation: {operation}, field: \"{field}\", fieldIndex: {field_index}, operatorIndex: {operator_index})"
                ),
            )
        };
        match operator.as_str() {
            "$in" | "$nin" => {
                let BsonValue::Array(values) = operand
                else {
                    return Err(bad("requires an array"));
                };
                if values.is_empty() {
                    return Err(bad("with an empty array is not defined by the MongoDB manual; refusing to guess"));
                }
            }
            "$all" => {
                if !matches!(operand, BsonValue::Array(_)) {
                    return Err(bad("requires an array"));
                }
            }
            "$exists" => {
                if !matches!(operand, BsonValue::Bool(_)) {
                    return Err(bad("requires a boolean"));
                }
            }
            "$size" => {
                if integer_of(operand).filter(|size| *size >= 0).is_none() {
                    return Err(bad("requires a non-negative integer"));
                }
            }
            "$mod" => {
                parse_mod_operand(operand).map_err(bad)?;
            }
            "$bitsAllSet" | "$bitsAnySet" | "$bitsAllClear" | "$bitsAnyClear" => {
                bit_mask(operand).map_err(bad)?;
            }
            "$regex" => {
                let BsonValue::String(pattern) = operand
                else {
                    return Err(bad("requires a string pattern"));
                };
                let options = match operators.get("$options") {
                    Some(BsonValue::String(options)) => options.as_str(),
                    _ => "",
                };
                // Compile during validation: a bad pattern or option must fail even
                // when the collection has no documents to scan.
                compile_regex(pattern, options).map_err(|error| bad(&error.message))?;
            }
            "$options" => {
                if !matches!(operand, BsonValue::String(_)) {
                    return Err(bad("requires a string"));
                }
                if !operators.fields.iter().any(|(name, _)| name == "$regex") {
                    return Err(bad("requires a $regex in the same operator document"));
                }
            }
            "$type" => {
                parse_type_operand(operand).map_err(|error| bad(&error.message))?;
            }
            "$elemMatch" => {
                let BsonValue::Document(specification) = operand
                else {
                    return Err(bad("requires a document"));
                };
                if has_operator(specification) {
                    validate_operator_document(specification, operation, field, field_index)?;
                }
                else {
                    validate_filter(specification, operation)?;
                }
            }
            "$not" => {
                let BsonValue::Document(specification) = operand
                else {
                    return Err(bad("requires an operator document"));
                };
                if !has_operator(specification) || specification.fields.iter().any(|(name, _)| !name.starts_with('$')) {
                    return Err(bad("requires an operator document"));
                }
                validate_operator_document(specification, operation, field, field_index)?;
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn unknown_operator(operator: &str, operation: &str, field: &str, field_index: usize, operator_index: usize) -> CommandError {
    CommandError::new(
        2,
        format!(
            "unknown operator: {operator} (operation: {operation}, field: \"{field}\", fieldIndex: {field_index}, operatorIndex: {operator_index})"
        ),
    )
}

fn empty_variables() -> ExpressionVariables {
    ExpressionVariables::new()
}

/// Evaluates a filter against one document with no bound variables.
pub fn matches(document: &BsonDocument, filter: &BsonDocument, expressions: &dyn ExpressionContext) -> Result<bool, CommandError> {
    matches_with_variables(document, filter, &empty_variables(), expressions)
}

/// Evaluates a filter against one document with the given aggregation variables.
pub fn matches_with_variables(
    document: &BsonDocument,
    filter: &BsonDocument,
    variables: &ExpressionVariables,
    expressions: &dyn ExpressionContext,
) -> Result<bool, CommandError> {
    for (field_index, (field, expected)) in filter.fields.iter().enumerate() {
        match field.as_str() {
            "$and" => {
                let BsonValue::Array(clauses) = expected
                else {
                    return Err(CommandError::new(2, format!("$and requires an array of filter documents (fieldIndex: {field_index})")));
                };
                for clause in clauses {
                    let BsonValue::Document(clause) = clause
                    else {
                        return Err(CommandError::new(2, format!("$and requires filter documents (fieldIndex: {field_index})")));
                    };
                    if !matches_with_variables(document, clause, variables, expressions)? {
                        return Ok(false);
                    }
                }
            }
            "$or" => {
                let BsonValue::Array(clauses) = expected
                else {
                    return Err(CommandError::new(2, format!("$or requires an array of filter documents (fieldIndex: {field_index})")));
                };
                let mut matched = false;
                for clause in clauses {
                    let BsonValue::Document(clause) = clause
                    else {
                        return Err(CommandError::new(2, format!("$or requires filter documents (fieldIndex: {field_index})")));
                    };
                    if matches_with_variables(document, clause, variables, expressions)? {
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    return Ok(false);
                }
            }
            "$nor" => {
                let BsonValue::Array(clauses) = expected
                else {
                    return Err(CommandError::new(2, format!("$nor requires an array of filter documents (fieldIndex: {field_index})")));
                };
                for clause in clauses {
                    let BsonValue::Document(clause) = clause
                    else {
                        return Err(CommandError::new(2, format!("$nor requires filter documents (fieldIndex: {field_index})")));
                    };
                    if matches_with_variables(document, clause, variables, expressions)? {
                        return Ok(false);
                    }
                }
            }
            "$expr" => {
                let value = expressions.evaluate(document, expected, variables)?;
                if !expression_truthy(&value) {
                    return Ok(false);
                }
            }
            _ if field.starts_with('$') => {
                return Err(CommandError::new(2, format!("unknown top level operator: {field} (fieldIndex: {field_index})")));
            }
            _ => {
                let candidates = collect_field(document, field);
                let matched = match expected {
                    BsonValue::Document(operators) if has_operator(operators) => {
                        operator_matches(&candidates, operators, field, field_index, expressions)?
                    }
                    _ => equality_candidates(&candidates, expected),
                };
                if !matched {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

/// Resolves a (possibly dotted) field path into every value the path selects.
///
/// A dotted segment below an array descends into every element, which is what
/// lets `a.b` match an array of documents; an all-digit segment indexes the
/// array instead. A missing path yields no candidates, which is how `$exists:
/// false`, `{a: null}` and `$nin` tell "absent" apart from "explicit null".
/// Resolves a (possibly dotted) field path into every value the path selects.
pub fn collect_field<'a>(document: &'a BsonDocument, path: &str) -> Vec<&'a BsonValue> {
    if let Some(value) = document.get(path) {
        return vec![value];
    }
    let segments: Vec<&str> = path.split('.').collect();
    let mut values = Vec::new();
    if let Some(first) = document.get(segments[0]) {
        descend(first, &segments[1..], &mut values);
    }
    values
}

pub(crate) fn descend<'a>(value: &'a BsonValue, segments: &[&str], values: &mut Vec<&'a BsonValue>) {
    if segments.is_empty() {
        values.push(value);
        return;
    }
    match value {
        BsonValue::Document(document) => {
            if let Some(next) = document.get(segments[0]) {
                descend(next, &segments[1..], values);
            }
        }
        BsonValue::Array(items) => {
            if let Ok(index) = segments[0].parse::<usize>() {
                if let Some(next) = items.get(index) {
                    descend(next, &segments[1..], values);
                }
            }
            else {
                for item in items {
                    descend(item, segments, values);
                }
            }
        }
        _ => {}
    }
}

/// Evaluates an operator document against the candidate values of one field.
pub fn operator_matches(
    candidates: &[&BsonValue],
    operators: &BsonDocument,
    field: &str,
    field_index: usize,
    expressions: &dyn ExpressionContext,
) -> Result<bool, CommandError> {
    let options = match operators.get("$options") {
        Some(BsonValue::String(options)) => options.as_str(),
        _ => "",
    };
    for (operator_index, (operator, operand)) in operators.fields.iter().enumerate() {
        if operator == "$options" {
            continue;
        }
        let matched = match operator.as_str() {
            "$eq" => equality_candidates(candidates, operand),
            "$ne" => !equality_candidates(candidates, operand),
            "$gt" => comparison_candidates(candidates, operand, |ordering| ordering == Ordering::Greater),
            "$gte" => comparison_candidates(candidates, operand, |ordering| ordering != Ordering::Less),
            "$lt" => comparison_candidates(candidates, operand, |ordering| ordering == Ordering::Less),
            "$lte" => comparison_candidates(candidates, operand, |ordering| ordering != Ordering::Greater),
            "$in" => in_candidates(candidates, operand),
            "$nin" => !in_candidates(candidates, operand),
            "$exists" => exists_candidates(candidates, operand),
            "$regex" => regex_candidates(candidates, operand, options)?,
            "$size" => size_candidates(candidates, operand),
            "$type" => type_candidates(candidates, operand)?,
            "$elemMatch" => elem_match_candidates(candidates, operand, expressions)?,
            "$all" => all_candidates(candidates, operand, expressions)?,
            "$not" => not_candidates(candidates, operand, expressions)?,
            "$mod" => mod_candidates(candidates, operand)?,
            "$bitsAllSet" => bits_candidates(candidates, operand, BitTest::AllSet)?,
            "$bitsAnySet" => bits_candidates(candidates, operand, BitTest::AnySet)?,
            "$bitsAllClear" => bits_candidates(candidates, operand, BitTest::AllClear)?,
            "$bitsAnyClear" => bits_candidates(candidates, operand, BitTest::AnyClear)?,
            other => return Err(unknown_operator(other, "filter", field, field_index, operator_index)),
        };
        if !matched {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `$eq` semantics over candidate values, including array element equality.
pub fn equality_candidates(candidates: &[&BsonValue], expected: &BsonValue) -> bool {
    if candidates.is_empty() {
        return matches!(expected, BsonValue::Null);
    }
    candidates.iter().any(|candidate| equality_one(candidate, expected))
}

pub(crate) fn equality_one(candidate: &BsonValue, expected: &BsonValue) -> bool {
    if values_equal(Some(candidate), expected) {
        return true;
    }
    match (candidate, expected) {
        (BsonValue::Array(items), _) => items.iter().any(|item| values_equal(Some(item), expected)),
        _ => false,
    }
}

pub(crate) fn in_candidates(candidates: &[&BsonValue], operand: &BsonValue) -> bool {
    let BsonValue::Array(values) = operand
    else {
        return false;
    };
    // One `$eq` predicate per list element, so `$in` containing `null` also
    // matches a missing field exactly like `{field: null}` does.
    values.iter().any(|value| equality_candidates(candidates, value))
}

pub(crate) fn exists_candidates(candidates: &[&BsonValue], operand: &BsonValue) -> bool {
    let BsonValue::Bool(exists) = operand
    else {
        return false;
    };
    candidates.is_empty() != *exists
}

pub(crate) fn comparison_candidates(candidates: &[&BsonValue], operand: &BsonValue, accept: impl Fn(Ordering) -> bool) -> bool {
    candidates.iter().any(|candidate| {
        if same_type_bracket(candidate, operand) && accept(compare_values(candidate, operand)) {
            return true;
        }
        if let BsonValue::Array(items) = candidate {
            return items.iter().any(|item| same_type_bracket(item, operand) && accept(compare_values(item, operand)));
        }
        false
    })
}

/// Comparison operators are type-bracketed: `{$gt: 5}` never matches a string,
/// even though strings sort after numbers in BSON order.
pub(crate) fn same_type_bracket(left: &BsonValue, right: &BsonValue) -> bool {
    value_rank(left) == value_rank(right)
}

pub(crate) fn regex_candidates(candidates: &[&BsonValue], operand: &BsonValue, options: &str) -> Result<bool, CommandError> {
    let BsonValue::String(pattern) = operand
    else {
        return Ok(false);
    };
    let regex = compile_regex(pattern, options)?;
    Ok(candidates.iter().any(|candidate| match candidate {
        BsonValue::String(text) => regex.is_match(text),
        BsonValue::Array(items) => items.iter().any(|item| matches!(item, BsonValue::String(text) if regex.is_match(text))),
        _ => false,
    }))
}

std::thread_local! {
    /// 最近编译过的 `$regex`，避免逐文档重编译。
    ///
    /// 匹配路径按文档逐条调用 `regex_candidates`，原实现每次都重新构造
    /// `regex::Regex`（编译一个前缀模式约十微秒，1000 篇文档就是十毫秒级）。
    /// 缓存只按 `(pattern, options)` 命中，超过上限整体清空，长生命周期进程
    /// 不会被任意用户模式撑大。
    static REGEX_CACHE: std::cell::RefCell<std::collections::HashMap<(String, String), regex::Regex>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

pub(crate) fn compile_regex(pattern: &str, options: &str) -> Result<regex::Regex, CommandError> {
    let key = (pattern.to_string(), options.to_string());
    if let Some(cached) = REGEX_CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return Ok(cached);
    }
    let mut builder = regex::RegexBuilder::new(pattern);
    for option in options.chars() {
        match option {
            'i' => {
                builder.case_insensitive(true);
            }
            'm' => {
                builder.multi_line(true);
            }
            's' => {
                builder.dot_matches_new_line(true);
            }
            'x' => {
                builder.ignore_whitespace(true);
            }
            // `u` is documented as accepted but redundant for UTF-8 strings.
            'u' => {}
            other => return Err(CommandError::new(2, format!("unsupported $regex option: {other}"))),
        }
    }
    let regex = builder.build().map_err(|error| CommandError::new(2, format!("invalid $regex pattern: {error}")))?;
    REGEX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(key, regex.clone());
    });
    Ok(regex)
}

pub(crate) fn size_candidates(candidates: &[&BsonValue], operand: &BsonValue) -> bool {
    let Some(size) = integer_of(operand)
    else {
        return false;
    };
    candidates.iter().any(|candidate| matches!(candidate, BsonValue::Array(items) if items.len() as i128 == size))
}

pub(crate) fn all_candidates(
    candidates: &[&BsonValue],
    operand: &BsonValue,
    expressions: &dyn ExpressionContext,
) -> Result<bool, CommandError> {
    let BsonValue::Array(values) = operand
    else {
        return Ok(false);
    };
    // The manual states an empty `$all` array matches no documents.
    if values.is_empty() {
        return Ok(false);
    }
    for candidate in candidates {
        let BsonValue::Array(items) = candidate
        else {
            if values.len() == 1 && equality_one(candidate, &values[0]) {
                return Ok(true);
            }
            continue;
        };
        let mut matches_all = true;
        for value in values {
            let satisfied = match value {
                BsonValue::Document(entry) if entry.fields.len() == 1 && entry.get("$elemMatch").is_some() => {
                    elem_match_candidates(&[*candidate], entry.get("$elemMatch").unwrap(), expressions)?
                }
                _ => items.iter().any(|item| values_equal(Some(item), value)),
            };
            if !satisfied {
                matches_all = false;
                break;
            }
        }
        if matches_all {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn elem_match_candidates(
    candidates: &[&BsonValue],
    operand: &BsonValue,
    expressions: &dyn ExpressionContext,
) -> Result<bool, CommandError> {
    let BsonValue::Document(specification) = operand
    else {
        return Ok(false);
    };
    let operator_form = has_operator(specification);
    for candidate in candidates {
        let BsonValue::Array(items) = candidate
        else {
            continue;
        };
        for item in items {
            let matched = if operator_form {
                operator_matches(&[item], specification, "filter", 0, expressions)?
            }
            else {
                match item {
                    BsonValue::Document(document) => matches(document, specification, expressions)?,
                    _ => false,
                }
            };
            if matched {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(crate) fn not_candidates(
    candidates: &[&BsonValue],
    operand: &BsonValue,
    expressions: &dyn ExpressionContext,
) -> Result<bool, CommandError> {
    let BsonValue::Document(specification) = operand
    else {
        return Ok(false);
    };
    Ok(!operator_matches(candidates, specification, "filter", 0, expressions)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeFilter {
    Number,
    Null,
    Bool,
    Int32,
    Int64,
    Double,
    String,
    Document,
    Array,
    Binary,
    ObjectId,
    Never,
}

pub(crate) fn parse_type_operand(operand: &BsonValue) -> Result<Vec<TypeFilter>, CommandError> {
    match operand {
        BsonValue::Array(values) => values.iter().map(parse_type_value).collect(),
        other => Ok(vec![parse_type_value(other)?]),
    }
}

pub(crate) fn parse_type_value(value: &BsonValue) -> Result<TypeFilter, CommandError> {
    if let BsonValue::String(name) = value {
        let filter = match name.as_str() {
            "double" => TypeFilter::Double,
            "string" => TypeFilter::String,
            "object" => TypeFilter::Document,
            "array" => TypeFilter::Array,
            "binData" => TypeFilter::Binary,
            "objectId" => TypeFilter::ObjectId,
            "bool" => TypeFilter::Bool,
            "null" => TypeFilter::Null,
            "int" => TypeFilter::Int32,
            "long" => TypeFilter::Int64,
            "number" => TypeFilter::Number,
            // Documented BSON types this store cannot hold: the query is valid
            // but can never match, exactly like a real server with no such value.
            "undefined"
            | "date"
            | "regex"
            | "dbPointer"
            | "javascript"
            | "symbol"
            | "javascriptWithScope"
            | "timestamp"
            | "decimal"
            | "minKey"
            | "maxKey" => TypeFilter::Never,
            other => return Err(CommandError::new(2, format!("unknown $type alias: {other}"))),
        };
        return Ok(filter);
    }
    let code = integer_of(value).ok_or_else(|| CommandError::new(2, "$type requires a type alias or a BSON type code"))?;
    let filter = match code {
        1 => TypeFilter::Double,
        2 => TypeFilter::String,
        3 => TypeFilter::Document,
        4 => TypeFilter::Array,
        5 => TypeFilter::Binary,
        7 => TypeFilter::ObjectId,
        8 => TypeFilter::Bool,
        10 => TypeFilter::Null,
        16 => TypeFilter::Int32,
        18 => TypeFilter::Int64,
        6 | 9 | 11 | 12 | 13 | 14 | 15 | 17 | 19 | -1 | 127 => TypeFilter::Never,
        other => return Err(CommandError::new(2, format!("unknown $type code: {other}"))),
    };
    Ok(filter)
}

pub(crate) fn type_candidates(candidates: &[&BsonValue], operand: &BsonValue) -> Result<bool, CommandError> {
    let filters = parse_type_operand(operand)?;
    let wanted = |value: &BsonValue| filters.iter().any(|filter| type_filter_matches(value, *filter));
    Ok(candidates.iter().any(|candidate| {
        if wanted(candidate) {
            return true;
        }
        match candidate {
            BsonValue::Array(items) => items.iter().any(&wanted),
            _ => false,
        }
    }))
}

pub(crate) fn type_filter_matches(value: &BsonValue, filter: TypeFilter) -> bool {
    match filter {
        TypeFilter::Number => matches!(value, BsonValue::Int32(_) | BsonValue::Int64(_) | BsonValue::Double(_)),
        TypeFilter::Null => matches!(value, BsonValue::Null),
        TypeFilter::Bool => matches!(value, BsonValue::Bool(_)),
        TypeFilter::Int32 => matches!(value, BsonValue::Int32(_)),
        TypeFilter::Int64 => matches!(value, BsonValue::Int64(_)),
        TypeFilter::Double => matches!(value, BsonValue::Double(_)),
        TypeFilter::String => matches!(value, BsonValue::String(_)),
        TypeFilter::Document => matches!(value, BsonValue::Document(_)),
        TypeFilter::Array => matches!(value, BsonValue::Array(_)),
        TypeFilter::Binary => matches!(value, BsonValue::Binary(_)),
        TypeFilter::ObjectId => matches!(value, BsonValue::ObjectId(_)),
        TypeFilter::Never => false,
    }
}

/// 解析 `$mod` 操作数：`[divisor, remainder]`。
///
/// 文案对齐 `mongod` 解析器的报错，并由单测钉住；确切措辞仍待真实
/// `mongod 8.0.9` 录制核对。
pub(crate) fn parse_mod_operand(operand: &BsonValue) -> Result<(&BsonValue, &BsonValue), &'static str> {
    let BsonValue::Array(parts) = operand
    else {
        return Err("malformed mod, needs to be an array");
    };
    if parts.len() < 2 {
        return Err("malformed mod, not enough elements");
    }
    if parts.len() > 2 {
        return Err("malformed mod, too many elements");
    }
    let Some(divisor) = number_of(&parts[0])
    else {
        return Err("malformed mod, divisor not a number");
    };
    if number_of(&parts[1]).is_none() {
        return Err("malformed mod, remainder not a number");
    }
    if divisor == 0.0 {
        return Err("divisor cannot be 0");
    }
    Ok((&parts[0], &parts[1]))
}

pub(crate) fn mod_candidates(candidates: &[&BsonValue], operand: &BsonValue) -> Result<bool, CommandError> {
    let (divisor, remainder) = parse_mod_operand(operand).map_err(|detail| CommandError::new(2, detail))?;
    Ok(candidates.iter().any(|candidate| mod_value_matches(candidate, divisor, remainder)))
}

pub(crate) fn mod_value_matches(candidate: &BsonValue, divisor: &BsonValue, remainder: &BsonValue) -> bool {
    if let BsonValue::Array(items) = candidate {
        return items.iter().any(|item| mod_value_matches(item, divisor, remainder));
    }
    if let (Some(value), Some(divisor), Some(remainder)) = (integral_i64(candidate), integral_i64(divisor), integral_i64(remainder)) {
        return value.checked_rem(divisor) == Some(remainder);
    }
    let (Some(value), Some(divisor), Some(remainder)) = (number_of(candidate), number_of(divisor), number_of(remainder))
    else {
        return false;
    };
    // `f64` 上的 `%` 就是 `fmod`，`mongod` 对 double 应用 `$mod` 也是这个语义。
    value % divisor == remainder
}

/// `$bits*` 系列算子各自的判定方式。
#[derive(Debug, Clone, Copy)]
pub(crate) enum BitTest {
    AllSet,
    AnySet,
    AllClear,
    AnyClear,
}

/// 解析位运算操作数：64 位二补码掩码，或显式位号列表。
/// `mongod` 把掩码文档化为有符号 64 位整数，因此负数会置起高位。
pub(crate) fn bit_mask(operand: &BsonValue) -> Result<u64, &'static str> {
    match operand {
        BsonValue::Int32(value) => Ok(i64::from(*value) as u64),
        BsonValue::Int64(value) => Ok(*value as u64),
        BsonValue::Double(value) if value.fract() == 0.0 && *value >= i64::MIN as f64 && *value <= i64::MAX as f64 => Ok(*value as i64 as u64),
        BsonValue::Array(positions) => {
            let mut mask = 0_u64;
            for position in positions {
                let Some(position) = integral_i64(position)
                else {
                    return Err("bit positions must be integers");
                };
                if !(0..64).contains(&position) {
                    return Err("bit positions must be between 0 and 63");
                }
                mask |= 1_u64 << (position as u32);
            }
            Ok(mask)
        }
        _ => Err("bitwise operators need an integer mask or an array of bit positions"),
    }
}

pub(crate) fn bits_candidates(candidates: &[&BsonValue], operand: &BsonValue, test: BitTest) -> Result<bool, CommandError> {
    let mask = bit_mask(operand).map_err(|detail| CommandError::new(2, detail))?;
    Ok(candidates.iter().any(|candidate| bits_value_matches(candidate, mask, test)))
}

pub(crate) fn bits_value_matches(candidate: &BsonValue, mask: u64, test: BitTest) -> bool {
    if let BsonValue::Array(items) = candidate {
        return items.iter().any(|item| bits_value_matches(item, mask, test));
    }
    let Some(bits) = integral_i64(candidate).map(|value| value as u64)
    else {
        return false;
    };
    match test {
        BitTest::AllSet => bits & mask == mask,
        BitTest::AnySet => bits & mask != 0,
        BitTest::AllClear => bits & mask == 0,
        BitTest::AnyClear => bits & mask != mask,
    }
}
