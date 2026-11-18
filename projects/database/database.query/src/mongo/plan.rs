//! MongoDB 索引选择与访问计划。
//!
//! 计划只消费 catalog 元数据（索引规格、命名空间是否存在、索引项是否落地都由
//! 调用方提供），不直接读存储；本模块产出可执行的 `HintedPlan` 与 `explain`
//! 需要的计划文档，执行层按计划扫描。

use database_formats::mongo::IndexSpec;
use database_protocol_mongo::bson::{BsonDocument, BsonValue, integral_value, value_to_key};

use crate::mongo::{
    CommandError,
    filter::{ExpressionContext, has_operator, matches, normalize_query},
    value::{count_value, hex_text, index_tuple, int_value, render_shell_value},
};

/// `hint` 需要的最小 catalog 视图。
///
/// 实现方（`database.execution::mongo`）负责读存储；计划只声明它需要的两个问题。
pub trait IndexCatalog {
    /// 命名空间是否存在。
    fn namespace_exists(&self, namespace: &str) -> Result<bool, CommandError>;

    /// 该命名空间的索引规格。
    fn index_specs(&self, namespace: &str) -> Result<Vec<IndexSpec>, CommandError>;
}

/// A `hint` resolved against the catalog.
#[derive(Debug, Clone)]
pub enum Hint {
    /// No hint, or `hint: {}` which `mongod 8.0.9` ignores.
    None,
    /// `{$natural: 1}` or `{$natural: -1}`.
    Natural(i32),
    /// A named or key-pattern hint that matched one visible index.
    Index(IndexSpec),
}

/// One equality, `$in` or range bound the executor actually probes on the leading key field.
#[derive(Debug, Clone)]
pub enum KeyBound {
    /// 首键字段没有可用边界，整段读取。
    Full,
    /// 首键字段等值。
    Eq(BsonValue),
    /// 首键字段命中 `$in` 列表。
    In(Vec<BsonValue>),
    /// 首键字段的数值范围；`bool` 表示闭区间，缺省一侧为无界。
    Range {
        /// 下界与是否闭区间。
        lower: Option<(BsonValue, bool)>,
        /// 上界与是否闭区间。
        upper: Option<(BsonValue, bool)>,
    },
}

/// Bounds rendered as `startKey` / `endKey` documents in plan output.
#[derive(Debug, Clone)]
pub struct CountRange {
    /// `startKey` 值。
    pub start: BsonValue,
    /// `startKeyInclusive`。
    pub start_inclusive: bool,
    /// `endKey` 值。
    pub end: BsonValue,
    /// `endKeyInclusive`。
    pub end_inclusive: bool,
}

/// 复合索引一个键字段的边界计划。
#[derive(Debug, Clone)]
pub struct FieldPlan {
    /// 键字段名。
    pub field: String,
    /// 探针边界。
    pub bound: KeyBound,
    /// `COUNT_SCAN` 报告的区间。
    pub count: CountRange,
}

/// 执行器真正消费的索引计划。
#[derive(Debug, Clone)]
pub struct HintedPlan {
    /// 命中的索引规格。
    pub spec: IndexSpec,
    /// 按键顺序的字段边界。
    pub fields: Vec<FieldPlan>,
    /// 索引没有完整消费、需要回表复核的条件。
    pub residual: BsonDocument,
    /// `forward` 或 `backward`。
    pub direction: &'static str,
    /// 排序是否由索引序覆盖。
    pub sort_covered: bool,
    /// 每个键字段是否出现过数组值。
    pub multi_key_fields: Vec<bool>,
}

impl HintedPlan {
    /// 任一键字段出现过数组值。
    pub fn is_multi_key(&self) -> bool {
        self.multi_key_fields.iter().any(|flag| *flag)
    }
}

/// 每个键字段是否在候选文档里出现过非空数组。
pub fn multi_key_fields(documents: &[BsonDocument], spec: &IndexSpec) -> Vec<bool> {
    spec.key.fields.iter().map(|(field, _)| field_holds_array(documents, field)).collect()
}

/// Resolves `hint` exactly as `mongod 8.0.9` does for `find` and `count`.
///
/// A hint against a namespace that does not exist is ignored (the command
/// simply returns no documents), while the same hint against an existing
/// collection fails. A named index must exist and must not be hidden; a key
/// pattern must match one index key document, including field order.
pub fn resolve_hint(
    catalog: &dyn IndexCatalog,
    hint: Option<&BsonValue>,
    namespace: &str,
    filter: &BsonDocument,
    sort: Option<&BsonValue>,
    projection: Option<&BsonValue>,
) -> Result<Hint, CommandError> {
    let Some(value) = hint
    else {
        return Ok(Hint::None);
    };
    match value {
        BsonValue::String(_) | BsonValue::Document(_) => {}
        _ => return Err(CommandError::new(9, "Hint must be a string or an object")),
    }
    if !catalog.namespace_exists(namespace)? {
        return Ok(Hint::None);
    }
    let planner_error = || hint_planner_error(namespace, filter, sort, projection);
    match value {
        BsonValue::String(name) => catalog
            .index_specs(namespace)?
            .into_iter()
            .find(|spec| spec.is_visible() && spec.name == *name)
            .map(Hint::Index)
            .ok_or_else(planner_error),
        BsonValue::Document(document) => {
            if document.fields.is_empty() {
                return Ok(Hint::None);
            }
            if let Some(direction) = document.get("$natural") {
                if document.fields.len() == 1
                    && let Some(number) = int_value(direction)
                    && (number == 1 || number == -1)
                {
                    return Ok(Hint::Natural(number as i32));
                }
                return Err(CommandError::new(2, "$natural hint cannot be set to a value other than -1 or 1."));
            }
            catalog
                .index_specs(namespace)?
                .into_iter()
                .find(|spec| spec.is_visible() && spec.key == *document)
                .map(Hint::Index)
                .ok_or_else(planner_error)
        }
        _ => Ok(Hint::None),
    }
}

pub(crate) fn hint_planner_error(
    namespace: &str,
    filter: &BsonDocument,
    sort: Option<&BsonValue>,
    projection: Option<&BsonValue>,
) -> CommandError {
    CommandError::new(
        2,
        format!(
            "error processing query: ns={namespace}Tree: {}\nSort: {}\nProj: {}\n planner returned error :: caused by :: hint provided does not correspond to an existing index",
            render_query_tree(filter),
            render_inline_document(sort),
            render_inline_document(projection)
        ),
    )
}

/// Renders the parsed filter the way the planner's `Tree:` line does.
pub(crate) fn render_query_tree(filter: &BsonDocument) -> String {
    let normalized = normalize_query(filter);
    if filter.fields.is_empty() {
        return "$and".to_string();
    }
    match normalized.get("$and") {
        Some(BsonValue::Array(clauses)) => {
            let mut text = "$and".to_string();
            for clause in clauses {
                if let BsonValue::Document(clause) = clause {
                    for line in render_clause_lines(clause) {
                        text.push_str("\n    ");
                        text.push_str(&line);
                    }
                }
            }
            text
        }
        _ => render_clause_lines(&normalized).join("\n    "),
    }
}

pub(crate) fn render_clause_lines(clause: &BsonDocument) -> Vec<String> {
    let mut lines = Vec::new();
    for (field, value) in &clause.fields {
        match value {
            BsonValue::Document(operators) if has_operator(operators) => {
                for (operator, operand) in &operators.fields {
                    if operator == "$options" {
                        continue;
                    }
                    lines.push(format!("{field} {operator} {}", render_tree_value(operand)));
                }
            }
            other => lines.push(format!("{field} $eq {}", render_tree_value(other))),
        }
    }
    lines
}

pub(crate) fn render_tree_value(value: &BsonValue) -> String {
    match value {
        BsonValue::String(text) => format!("{text:?}"),
        BsonValue::Array(items) => {
            format!("[ {}]", items.iter().map(render_tree_value).collect::<Vec<_>>().join(" "))
        }
        BsonValue::Document(document) => render_inline_document(Some(&BsonValue::Document(document.clone()))),
        _ => scalar_text(value),
    }
}

/// `{ a: -1 }` / `{}` rendering used by the `Sort:` and `Proj:` lines.
pub(crate) fn render_inline_document(value: Option<&BsonValue>) -> String {
    match value {
        Some(BsonValue::Document(document)) if !document.fields.is_empty() => format!(
            "{{ {} }}",
            document.fields.iter().map(|(field, value)| format!("{field}: {}", render_tree_value(value))).collect::<Vec<_>>().join(", ")
        ),
        _ => "{}".to_string(),
    }
}

pub(crate) fn scalar_text(value: &BsonValue) -> String {
    match value {
        BsonValue::Int32(number) => number.to_string(),
        BsonValue::Int64(number) => number.to_string(),
        BsonValue::Double(number) => {
            if number.is_nan() {
                "nan.0".to_string()
            }
            else if number.is_infinite() {
                if number.is_sign_positive() { "inf.0".to_string() } else { "-inf.0".to_string() }
            }
            else {
                format!("{number:?}")
            }
        }
        BsonValue::Decimal128(number) => number.as_str(),
        BsonValue::String(text) => format!("{text:?}"),
        BsonValue::Bool(flag) => flag.to_string(),
        BsonValue::Null => "null".to_string(),
        BsonValue::Undefined => "undefined".to_string(),
        BsonValue::MinKey => "MinKey".to_string(),
        BsonValue::MaxKey => "MaxKey".to_string(),
        BsonValue::ObjectId(bytes) => format!("ObjectId('{}')", hex_text(bytes)),
        BsonValue::Binary(binary) => format!("BinData({}, {})", binary.subtype, hex_text(&binary.bytes)),
        BsonValue::DateTime(milliseconds) => format!("new Date({milliseconds})"),
        BsonValue::Timestamp { t, i } => format!("Timestamp({t}, {i})"),
        BsonValue::Symbol(text) => format!("{text:?}"),
        BsonValue::Regex { pattern, options } => format!("/{pattern}/{options}"),
        BsonValue::Code(code) => code.clone(),
        BsonValue::CodeWithScope { code, scope } => {
            format!("CodeWScope( {code}, {} )", render_shell_value(&BsonValue::Document(scope.clone())))
        }
        BsonValue::DbPointer { namespace, id } => {
            format!("DBPointer( \"{namespace}\", ObjectId('{}') )", hex_text(id))
        }
        other => format!("{other:?}"),
    }
}

pub(crate) fn min_key_value() -> BsonValue {
    let mut document = BsonDocument::new();
    document.push("$minKey".to_string(), BsonValue::Int32(1));
    BsonValue::Document(document)
}

pub(crate) fn max_key_value() -> BsonValue {
    let mut document = BsonDocument::new();
    document.push("$maxKey".to_string(), BsonValue::Int32(1));
    BsonValue::Document(document)
}

pub(crate) fn full_count_range() -> CountRange {
    CountRange { start: min_key_value(), start_inclusive: true, end: max_key_value(), end_inclusive: true }
}

pub(crate) fn field_holds_array(documents: &[BsonDocument], field: &str) -> bool {
    documents.iter().any(|document| match document.get(field) {
        Some(BsonValue::Array(items)) => !items.is_empty(),
        _ => false,
    })
}

/// 可执行的首键字段边界。
///
/// 等值、`$in` 与整数边界的范围可以直接切成索引项区间。范围只接受整数边界：
/// 索引项键把整数值编码成定长保序键，非整数（double 位模式、decimal 系数）在
/// 现有编码里不保序，硬切区间会漏掉落在区间外的元素，因此如实退回 `COLLSCAN`。
pub(crate) fn executable_field_plan(field: &str, predicate: Option<&BsonValue>) -> FieldPlan {
    let full = || FieldPlan { field: field.to_string(), bound: KeyBound::Full, count: full_count_range() };
    let equality = |value: &BsonValue| -> Option<FieldPlan> {
        if matches!(value, BsonValue::Regex { .. }) || matches!(value, BsonValue::Document(document) if has_operator(document)) {
            return None;
        }
        Some(FieldPlan {
            field: field.to_string(),
            bound: KeyBound::Eq(value.clone()),
            count: CountRange { start: value.clone(), start_inclusive: true, end: value.clone(), end_inclusive: true },
        })
    };
    let range = |operators: &BsonDocument| -> Option<FieldPlan> {
        let mut lower: Option<(BsonValue, bool)> = None;
        let mut upper: Option<(BsonValue, bool)> = None;
        for (operator, operand) in &operators.fields {
            let (slot, inclusive) = match operator.as_str() {
                "$gt" => (&mut lower, false),
                "$gte" => (&mut lower, true),
                "$lt" => (&mut upper, false),
                "$lte" => (&mut upper, true),
                _ => return None,
            };
            numeric_range_bound(operand)?;
            *slot = Some((operand.clone(), inclusive));
        }
        if lower.is_none() && upper.is_none() {
            return None;
        }
        // 数值范围缺边即 ±无穷，`COUNT_SCAN` 的 `startKey`/`endKey` 如实渲染。
        let count = CountRange {
            start: lower.as_ref().map(|(value, _)| value.clone()).unwrap_or_else(|| BsonValue::Double(f64::NEG_INFINITY)),
            start_inclusive: lower.as_ref().is_none_or(|(_, inclusive)| *inclusive),
            end: upper.as_ref().map(|(value, _)| value.clone()).unwrap_or_else(|| BsonValue::Double(f64::INFINITY)),
            end_inclusive: upper.as_ref().is_none_or(|(_, inclusive)| *inclusive),
        };
        Some(FieldPlan { field: field.to_string(), bound: KeyBound::Range { lower, upper }, count })
    };
    let Some(predicate) = predicate
    else {
        return full();
    };
    match predicate {
        BsonValue::Document(operators) if has_operator(operators) => {
            if operators.fields.len() == 1 {
                if let Some(value) = operators.get("$eq")
                    && let Some(plan) = equality(value)
                {
                    return plan;
                }
                if let Some(BsonValue::Array(values)) = operators.get("$in")
                    && values.iter().all(|value| {
                        !matches!(value, BsonValue::Regex { .. }) && !matches!(value, BsonValue::Document(document) if has_operator(document))
                    })
                {
                    return FieldPlan { field: field.to_string(), bound: KeyBound::In(values.clone()), count: full_count_range() };
                }
            }
            range(operators).unwrap_or_else(full)
        }
        BsonValue::Regex { .. } => full(),
        other => equality(other).unwrap_or_else(full),
    }
}

/// 范围边界里能精确落到整数键上的值；非整数边界不进索引区间。
pub(crate) fn numeric_range_bound(value: &BsonValue) -> Option<i64> {
    match value {
        BsonValue::Int32(number) => Some(*number as i64),
        BsonValue::Int64(number) => Some(*number),
        BsonValue::Double(number) => integral_value(*number),
        BsonValue::Decimal128(number) => number.as_i64(),
        _ => None,
    }
}

/// 构建执行器真正会消费的索引计划。
///
/// 首个键字段按谓词切探针边界；前面字段全是等值时，后续字段的等值谓词继续
/// 收窄同一前缀，遇到范围或非谓词字段即停。边界已完整消费的谓词从 `residual`
/// 移除，`explain` 的 `FETCH.filter` 只报告回表复核真正还要再查的条件。排序与
/// 索引键完全同向或反向时按索引序交付（反向即 `backward` 扫描），否则内存排序。
/// hint 指向部分索引而查询不被 `partialFilterExpression` 蕴含时，索引域与查询
/// 域不相交，按全范围扫描整个部分索引，与 `mongod` 的 hint 行为一致。
pub fn build_hinted_plan(
    multi_key_fields: &[bool],
    spec: &IndexSpec,
    filter: &BsonDocument,
    sort: &[(String, i32)],
    expressions: &dyn ExpressionContext,
) -> HintedPlan {
    let mut fields = Vec::with_capacity(spec.key.fields.len());
    let mut consumed: Vec<String> = Vec::new();
    let partial_outside = spec.partial_filter_expression.as_ref().is_some_and(|partial| !query_implies_partial(filter, partial, expressions));
    if partial_outside {
        for (field, _) in &spec.key.fields {
            fields.push(FieldPlan { field: field.clone(), bound: KeyBound::Full, count: full_count_range() });
        }
    }
    else {
        let mut narrowing = true;
        for (position, (field, _)) in spec.key.fields.iter().enumerate() {
            if position == 0 {
                let plan = executable_field_plan(field, filter.get(field));
                if !matches!(plan.bound, KeyBound::Full) {
                    consumed.push(field.clone());
                }
                narrowing = matches!(plan.bound, KeyBound::Eq(_));
                fields.push(plan);
            }
            else if narrowing {
                match equality_bound(filter.get(field)) {
                    Some(value) => {
                        consumed.push(field.clone());
                        fields.push(FieldPlan {
                            field: field.clone(),
                            bound: KeyBound::Eq(value.clone()),
                            count: equality_count_range(&value),
                        });
                    }
                    None => {
                        narrowing = false;
                        fields.push(FieldPlan { field: field.clone(), bound: KeyBound::Full, count: full_count_range() });
                    }
                }
            }
            else {
                fields.push(FieldPlan { field: field.clone(), bound: KeyBound::Full, count: full_count_range() });
            }
        }
    }
    let mut residual = BsonDocument::new();
    for (field, value) in &filter.fields {
        if !field.starts_with('$') && consumed.iter().any(|consumed| consumed == field) {
            continue;
        }
        // `FETCH.filter` 报告解析后的形态：裸等值渲染成 `$eq`。
        let normalized = match value {
            BsonValue::Document(operators) if has_operator(operators) => value.clone(),
            other => {
                let mut equality = BsonDocument::new();
                equality.push("$eq".to_string(), other.clone());
                BsonValue::Document(equality)
            }
        };
        residual.push(field.clone(), normalized);
    }
    let key_matches = |reverse: bool| {
        sort.len() == spec.key.fields.len()
            && sort.iter().zip(spec.key.fields.iter()).all(|((sort_field, sort_direction), (field, key_direction))| {
                sort_field == field
                    && int_value(key_direction).is_some_and(|key| i64::from(*sort_direction) == if reverse { -key } else { key })
            })
    };
    HintedPlan {
        spec: spec.clone(),
        fields,
        residual,
        direction: if key_matches(true) { "backward" } else { "forward" },
        sort_covered: sort.is_empty() || key_matches(false) || key_matches(true),
        multi_key_fields: multi_key_fields.to_vec(),
    }
}

/// 复合索引前缀可用的等值谓词值；非等值返回 `None`。
pub(crate) fn equality_bound(predicate: Option<&BsonValue>) -> Option<BsonValue> {
    match predicate? {
        BsonValue::Document(document) if has_operator(document) => {
            if document.fields.len() == 1 {
                return document.get("$eq").cloned();
            }
            None
        }
        BsonValue::Regex { .. } => None,
        other => Some(other.clone()),
    }
}

pub(crate) fn equality_count_range(value: &BsonValue) -> CountRange {
    CountRange { start: value.clone(), start_inclusive: true, end: value.clone(), end_inclusive: true }
}

/// `partialFilterExpression` 的每个字段都要求查询里有一条同字段条件，且该
/// 条件落在部分索引域内（等值直接对表达式求值，复杂条件要求逐字相同）；
/// 不满足就不用这个部分索引。
pub(crate) fn query_implies_partial(filter: &BsonDocument, partial: &BsonDocument, expressions: &dyn ExpressionContext) -> bool {
    partial.fields.iter().all(|(field, expected)| {
        let Some(actual) = filter.get(field)
        else {
            return false;
        };
        match plain_equality(actual) {
            Some(plain) => {
                let mut probe = BsonDocument::new();
                probe.push(field.clone(), plain.clone());
                let mut clause = BsonDocument::new();
                clause.push(field.clone(), expected.clone());
                matches(&probe, &clause, expressions).unwrap_or(false)
            }
            None => actual == expected,
        }
    })
}

pub(crate) fn plain_equality(value: &BsonValue) -> Option<&BsonValue> {
    match value {
        BsonValue::Document(operators) if has_operator(operators) => operators.get("$eq"),
        other => Some(other),
    }
}

/// 首个键字段的探针值；全范围扫描返回 `None`。
pub(crate) fn plan_probes(plan: &HintedPlan) -> Option<Vec<BsonValue>> {
    match plan.fields.first().map(|field| &field.bound) {
        Some(KeyBound::Eq(value)) => Some(vec![value.clone()]),
        Some(KeyBound::In(values)) => Some(values.clone()),
        _ => None,
    }
}

/// 一次索引读取的区间：前缀加上排他游标。
pub struct IndexSpan {
    /// 前缀字节。
    pub prefix: Vec<u8>,
    /// 排他下界。
    pub after: Option<Vec<u8>>,
    /// 排他上界。
    pub before: Option<Vec<u8>>,
}

/// 首键字段要读的索引区间；`None` 表示整段读取（无谓词的 hint 全索引扫描）。
///
/// 前面字段全是等值时，探针前缀沿索引键逐个拼接，复合等值一次落到区间上。
/// 首键字段要读的索引区间；`None` 表示整段读取。
pub fn plan_spans(plan: &HintedPlan) -> Option<Vec<IndexSpan>> {
    match plan.fields.first().map(|field| &field.bound)? {
        KeyBound::Full => None,
        KeyBound::Eq(_) => {
            let prefix: Vec<BsonValue> = plan
                .fields
                .iter()
                .take_while(|field| matches!(field.bound, KeyBound::Eq(_)))
                .filter_map(|field| match &field.bound {
                    KeyBound::Eq(value) => Some(value.clone()),
                    _ => None,
                })
                .collect();
            Some(vec![IndexSpan { prefix: index_tuple(&prefix), after: None, before: None }])
        }
        KeyBound::In(values) => Some(values.iter().map(equality_span).collect()),
        KeyBound::Range { lower, upper } => Some(range_spans(lower.as_ref(), upper.as_ref())),
    }
}

pub(crate) fn equality_span(value: &BsonValue) -> IndexSpan {
    IndexSpan { prefix: index_tuple(std::slice::from_ref(value)), after: None, before: None }
}

/// 整数范围切出定长保序区间，非整数 double / decimal 整段读取后由回表过滤收敛。
///
/// 索引项键是「键长度（4 字节）+ 键字节 + 文档键」，整数键固定 9 字节，
/// 因此整数区间可以按长度前缀定位；double 位模式与 decimal 系数在现有编码里
/// 不保序，硬切会漏数据，只能整段读。
pub(crate) fn range_spans(lower: Option<&(BsonValue, bool)>, upper: Option<&(BsonValue, bool)>) -> Vec<IndexSpan> {
    let mut spans = Vec::new();
    let start = match lower {
        None => Ok(None),
        Some((value, inclusive)) => match numeric_range_bound(value) {
            Some(number) if *inclusive => Ok(Some(number)),
            Some(number) => number.checked_add(1).map(Some).ok_or(()),
            None => Err(()),
        },
    };
    let end = match upper {
        None => Ok(None),
        Some((value, inclusive)) => match numeric_range_bound(value) {
            Some(number) if *inclusive => Ok(Some(number)),
            Some(number) => number.checked_sub(1).map(Some).ok_or(()),
            None => Err(()),
        },
    };
    if let (Ok(start), Ok(end)) = (start, end)
        && (start.is_none() || end.is_none() || start <= end)
    {
        let entry_length = 9u32;
        let region = numeric_entry_prefix(b'n', entry_length as usize);
        let mut span = IndexSpan { prefix: region, after: None, before: None };
        if let Some(start) = start {
            let mut key = entry_length.to_be_bytes().to_vec();
            key.extend_from_slice(&value_to_key(&BsonValue::Int64(start)));
            span.after = Some(key);
        }
        if let Some(end) = end {
            let mut key = entry_length.to_be_bytes().to_vec();
            key.extend_from_slice(&value_to_key(&BsonValue::Int64(end)));
            span.before = prefix_successor(&key);
        }
        spans.push(span);
    }
    spans.push(IndexSpan { prefix: numeric_entry_prefix(b'd', 9), after: None, before: None });
    spans.push(IndexSpan { prefix: numeric_entry_prefix(b'D', 22), after: None, before: None });
    spans
}

/// 单字段索引项的前缀：键长度（4 字节大端）+ 类型字节。
pub(crate) fn numeric_entry_prefix(type_byte: u8, key_len: usize) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(5);
    prefix.extend_from_slice(&(key_len as u32).to_be_bytes());
    prefix.push(type_byte);
    prefix
}

/// 严格大于所有以 `prefix` 开头的键的最小键；全 `0xFF` 时返回 `None`。
pub(crate) fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut successor = prefix.to_vec();
    for position in (0..successor.len()).rev() {
        if successor[position] == u8::MAX {
            successor.truncate(position);
        }
        else {
            successor[position] += 1;
            return Some(successor);
        }
    }
    None
}

/// 没有显式 hint 时按过滤条件挑候选索引：只看首个键字段上的等值/`$in`。
///
/// 返回按 catalog 顺序排列的可执行索引；索引项是否已经落地由调用方核对。
pub fn automatic_candidates(specs: &[IndexSpec], filter: &BsonDocument, expressions: &dyn ExpressionContext) -> Vec<IndexSpec> {
    let candidates: Vec<String> = filter.fields.iter().filter(|(field, _)| !field.starts_with('$')).map(|(field, _)| field.clone()).collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    let mut chosen = Vec::new();
    for specification in specs {
        let Some((first, _)) = specification.key.fields.first()
        else {
            continue;
        };
        if !candidates.iter().any(|field| field == first) {
            continue;
        }
        let plan = build_hinted_plan(&[], specification, filter, &[], expressions);
        if plan_spans(&plan).is_some() && index_executable(&plan, filter, false, expressions) {
            chosen.push(specification.clone());
        }
    }
    chosen
}

/// 这个索引计划能否被当前执行器如实执行。
///
/// 排序规则、稀疏索引的 null 等值、无法被查询蕴含的部分索引都会让索引项与
/// 匹配语义不再一一对应，遇到这些情况一律退回 `COLLSCAN`。显式 `hint` 指向
/// 部分索引时例外：`mongod` 照样扫描整个部分索引，`build_hinted_plan` 已把
/// 边界收敛为全范围。
pub fn index_executable(plan: &HintedPlan, filter: &BsonDocument, explicit_hint: bool, expressions: &dyn ExpressionContext) -> bool {
    let specification = &plan.spec;
    if !specification.is_visible() || specification.collation.is_some() {
        return false;
    }
    // 数组字段上两侧范围可以分别由不同元素满足，单元素区间会漏掉
    // `{a: [3, 12]}` 这类文档。标记为 multikey 后只允许单侧范围。
    if specification.multikey
        && matches!(plan.fields.first().map(|field| &field.bound), Some(KeyBound::Range { lower: Some(_), upper: Some(_) }))
    {
        return false;
    }
    if specification.is_sparse() {
        let Some(values) = plan_probes(plan)
        else {
            return false;
        };
        if values.iter().any(|value| matches!(value, BsonValue::Null | BsonValue::Undefined)) {
            return false;
        }
    }
    if !explicit_hint
        && let Some(partial) = &specification.partial_filter_expression
        && !query_implies_partial(filter, partial, expressions)
    {
        return false;
    }
    true
}

pub(crate) fn bound_text(bound: &KeyBound, backward: bool) -> Vec<String> {
    match bound {
        KeyBound::Full => vec![if backward { "[MaxKey, MinKey]".to_string() } else { "[MinKey, MaxKey]".to_string() }],
        KeyBound::Eq(value) => vec![format!("[{}, {}]", scalar_text(value), scalar_text(value))],
        KeyBound::In(values) => values.iter().map(|value| format!("[{}, {}]", scalar_text(value), scalar_text(value))).collect(),
        KeyBound::Range { lower, upper } => {
            let (start_value, start_inclusive) = match lower {
                Some((value, inclusive)) => (scalar_text(value), *inclusive),
                None => ("-inf.0".to_string(), true),
            };
            let (end_value, end_inclusive) = match upper {
                Some((value, inclusive)) => (scalar_text(value), *inclusive),
                None => ("inf.0".to_string(), true),
            };
            let (first, first_inclusive, last, last_inclusive) = if backward {
                (end_value, end_inclusive, start_value, start_inclusive)
            }
            else {
                (start_value, start_inclusive, end_value, end_inclusive)
            };
            vec![format!("{}{}, {}{}", if first_inclusive { "[" } else { "(" }, first, last, if last_inclusive { "]" } else { ")" })]
        }
    }
}

pub(crate) fn plan_key_pattern(plan: &HintedPlan) -> BsonDocument {
    plan.spec.key.clone()
}

pub(crate) fn plan_multi_key_paths(plan: &HintedPlan) -> BsonDocument {
    let mut paths = BsonDocument::new();
    for (position, (field, _)) in plan.spec.key.fields.iter().enumerate() {
        let mut values = Vec::new();
        if plan.multi_key_fields.get(position).copied().unwrap_or(false) {
            values.push(BsonValue::String(field.clone()));
        }
        paths.push(field.clone(), BsonValue::Array(values));
    }
    paths
}

/// `IXSCAN` / `COUNT_SCAN` shared metadata.
pub fn scan_stage_document(plan: &HintedPlan, count_scan: bool) -> BsonDocument {
    let mut stage = BsonDocument::new();
    stage.push("stage".to_string(), BsonValue::String(if count_scan { "COUNT_SCAN" } else { "IXSCAN" }.to_string()));
    stage.push("keyPattern".to_string(), BsonValue::Document(plan_key_pattern(plan)));
    stage.push("indexName".to_string(), BsonValue::String(plan.spec.name.clone()));
    stage.push("isMultiKey".to_string(), BsonValue::Bool(plan.is_multi_key()));
    stage.push("multiKeyPaths".to_string(), BsonValue::Document(plan_multi_key_paths(plan)));
    stage.push("isUnique".to_string(), BsonValue::Bool(plan.spec.unique));
    stage.push("isSparse".to_string(), BsonValue::Bool(plan.spec.is_sparse()));
    stage.push("isPartial".to_string(), BsonValue::Bool(plan.spec.partial_filter_expression.is_some()));
    stage.push("indexVersion".to_string(), BsonValue::Int32(2));
    if count_scan {
        let mut bounds = BsonDocument::new();
        let mut start = BsonDocument::new();
        let mut end = BsonDocument::new();
        let mut start_inclusive = true;
        let mut end_inclusive = true;
        for field in &plan.fields {
            start.push(field.field.clone(), field.count.start.clone());
            end.push(field.field.clone(), field.count.end.clone());
            start_inclusive &= field.count.start_inclusive;
            end_inclusive &= field.count.end_inclusive;
        }
        bounds.push("startKey".to_string(), BsonValue::Document(start));
        bounds.push("startKeyInclusive".to_string(), BsonValue::Bool(start_inclusive));
        bounds.push("endKey".to_string(), BsonValue::Document(end));
        bounds.push("endKeyInclusive".to_string(), BsonValue::Bool(end_inclusive));
        stage.push("indexBounds".to_string(), BsonValue::Document(bounds));
    }
    else {
        stage.push("direction".to_string(), BsonValue::String(plan.direction.to_string()));
        let mut bounds = BsonDocument::new();
        for field in &plan.fields {
            let backward = plan.direction == "backward";
            bounds.push(field.field.clone(), BsonValue::Array(bound_text(&field.bound, backward).into_iter().map(BsonValue::String).collect()));
        }
        stage.push("indexBounds".to_string(), BsonValue::Document(bounds));
    }
    stage
}

pub(crate) fn add_fetch_wrapper(stage: BsonDocument, filter: &BsonDocument) -> BsonDocument {
    let mut fetch = BsonDocument::new();
    fetch.push("stage".to_string(), BsonValue::String("FETCH".to_string()));
    if !filter.fields.is_empty() {
        fetch.push("filter".to_string(), BsonValue::Document(filter.clone()));
    }
    fetch.push("inputStage".to_string(), BsonValue::Document(stage));
    fetch
}

/// `winningPlan` for a hint-driven `find`.
pub fn hinted_find_plan(plan: &HintedPlan, skip: usize, limit: usize, sort: &[(String, i32)]) -> BsonDocument {
    let pushed = skip > 0 && plan.residual.fields.is_empty() && (sort.is_empty() || plan.sort_covered);
    let mut scan = scan_stage_document(plan, false);
    if pushed {
        // `skip` 下推到索引层：`SKIP` 夹在 `IXSCAN` 与 `FETCH` 之间，跳过的命中不回表。
        let mut skipped = BsonDocument::new();
        skipped.push("stage".to_string(), BsonValue::String("SKIP".to_string()));
        skipped.push("skipAmount".to_string(), count_value(skip as i64));
        skipped.push("inputStage".to_string(), BsonValue::Document(scan));
        scan = skipped;
    }
    let mut stage = add_fetch_wrapper(scan, &plan.residual);
    if !plan.sort_covered {
        let mut pattern = BsonDocument::new();
        for (field, direction) in sort {
            pattern.push(field.clone(), BsonValue::Int32(*direction));
        }
        let mut sorted = BsonDocument::new();
        sorted.push("stage".to_string(), BsonValue::String("SORT".to_string()));
        sorted.push("sortPattern".to_string(), BsonValue::Document(pattern));
        sorted.push("memLimit".to_string(), BsonValue::Int32(104857600));
        sorted.push("type".to_string(), BsonValue::String("simple".to_string()));
        sorted.push("inputStage".to_string(), BsonValue::Document(stage));
        stage = sorted;
    }
    if skip > 0 && !pushed {
        let mut skipped = BsonDocument::new();
        skipped.push("stage".to_string(), BsonValue::String("SKIP".to_string()));
        skipped.push("skipAmount".to_string(), count_value(skip as i64));
        skipped.push("inputStage".to_string(), BsonValue::Document(stage));
        stage = skipped;
    }
    if limit > 0 {
        let mut limited = BsonDocument::new();
        limited.push("stage".to_string(), BsonValue::String("LIMIT".to_string()));
        limited.push("limitAmount".to_string(), count_value(limit as i64));
        limited.push("inputStage".to_string(), BsonValue::Document(stage));
        stage = limited;
    }
    let mut cached = BsonDocument::new();
    cached.push("isCached".to_string(), BsonValue::Bool(false));
    for (field, value) in stage.fields {
        cached.push(field, value);
    }
    cached
}

/// `winningPlan` for a hint-driven `count`.
pub fn hinted_count_plan(plan: &HintedPlan) -> BsonDocument {
    // 边界完整消费查询时计数不回表，`mongod` 用 `COUNT_SCAN` 直接数索引项。
    let stage = if plan.residual.fields.is_empty() {
        scan_stage_document(plan, true)
    }
    else {
        add_fetch_wrapper(scan_stage_document(plan, false), &plan.residual)
    };
    let mut counted = BsonDocument::new();
    counted.push("stage".to_string(), BsonValue::String("COUNT".to_string()));
    counted.push("inputStage".to_string(), BsonValue::Document(stage));
    let mut cached = BsonDocument::new();
    cached.push("isCached".to_string(), BsonValue::Bool(false));
    for (field, value) in counted.fields {
        cached.push(field, value);
    }
    cached
}

/// `winningPlan` for a collection scan with optional skip 和 limit.
pub fn winning_plan(command: &str, filter: &BsonDocument, skip: usize, limit: usize, direction: &str) -> BsonDocument {
    let mut scan = BsonDocument::new();
    scan.push("stage".to_string(), BsonValue::String("COLLSCAN".to_string()));
    if !filter.fields.is_empty() {
        scan.push("filter".to_string(), BsonValue::Document(filter.clone()));
    }
    scan.push("direction".to_string(), BsonValue::String(direction.to_string()));
    let mut stage = if command == "count" {
        let mut counted = BsonDocument::new();
        counted.push("stage".to_string(), BsonValue::String("COUNT".to_string()));
        counted.push("inputStage".to_string(), BsonValue::Document(scan));
        counted
    }
    else {
        scan
    };
    if command == "find" {
        if skip > 0 {
            let mut skipped = BsonDocument::new();
            skipped.push("stage".to_string(), BsonValue::String("SKIP".to_string()));
            skipped.push("skipAmount".to_string(), count_value(skip as i64));
            skipped.push("inputStage".to_string(), BsonValue::Document(stage));
            stage = skipped;
        }
        if limit > 0 {
            let mut limited = BsonDocument::new();
            limited.push("stage".to_string(), BsonValue::String("LIMIT".to_string()));
            limited.push("limitAmount".to_string(), count_value(limit as i64));
            limited.push("inputStage".to_string(), BsonValue::Document(stage));
            stage = limited;
        }
    }
    // `isCached` marks only the outermost stage; nested input stages omit it.
    let mut cached = BsonDocument::new();
    cached.push("isCached".to_string(), BsonValue::Bool(false));
    for (field, value) in stage.fields {
        cached.push(field, value);
    }
    cached
}
