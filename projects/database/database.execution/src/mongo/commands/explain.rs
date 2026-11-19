//! `explain` 装配：三档 verbosity、执行统计与稳定哈希字段。

use super::*;

/// `explain` for `find` and `count`, covering all three verbosity levels.
///
/// The plan comes from the same access decision `find`/`count` use, and the
/// execution figures come from the real scan: `totalDocsExamined` counts the
/// documents this run read, and the timings are measured on the monotonic clock.
/// Hash fields are derived from the normalized query shape and the actual
/// winning plan; server identity reports this implementation rather than a
/// `mongod` version string.
pub fn explain(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let verbosity = match command.get("verbosity") {
        Some(BsonValue::String(verbosity)) => {
            if !["queryPlanner", "executionStats", "allPlansExecution"].contains(&verbosity.as_str()) {
                return Err(CommandError::new(
                    2,
                    format!("Enumeration value '{verbosity}' for field 'explain.verbosity' is not a valid value."),
                ));
            }
            verbosity.clone()
        }
        Some(_) => return Err(CommandError::new(2, "explain verbosity must be a string")),
        None => "executionStats".to_string(),
    };
    let Some(BsonValue::Document(inner)) = command.get("explain")
    else {
        return Err(CommandError::new(2, "explain requires a document"));
    };
    let Some(name) = inner.command_name()
    else {
        return Err(CommandError::new(2, "explain requires a command"));
    };
    if name != "find" && name != "count" {
        return Err(CommandError::new(2, format!("explain is not supported for `{name}`")));
    }

    let database = match command.get("$db") {
        Some(BsonValue::String(database)) => database.clone(),
        _ => "test".to_string(),
    };
    let collection = match inner.get(name) {
        Some(BsonValue::String(collection)) => collection.clone(),
        _ => return Err(CommandError::new(2, format!("{name} requires a collection name"))),
    };
    let namespace = format!("{database}.{collection}");
    let filter = match inner.get(if name == "find" { "filter" } else { "query" }) {
        Some(BsonValue::Document(filter)) => filter.clone(),
        Some(_) => return Err(CommandError::new(2, format!("{name} query must be a document"))),
        None => BsonDocument::new(),
    };
    validate_filter(&filter, "explain")?;
    let hint = resolve_hint(inner, store, &namespace, &filter, inner.get("sort"), inner.get("projection"))?;
    let (skip, limit) = explain_skip_limit(inner)?;
    let normalized = normalize_query(&filter);
    let sort = if name == "find" { parse_sort(inner.get("sort")) } else { Vec::new() };

    // 与 find / count 走同一个访问路径决策函数，并且和它们一样不为了选路物化
    // 整个集合：`choose_access` 需要的是多键字段线索，find/count 传空切片，
    // explain 也必须给同一输入，否则两条路径会选出不同计划。
    let planning_started = std::time::Instant::now();
    let access = choose_access(store, &namespace, &filter, &hint, &sort, &[], None)?;
    // 与 find / count 用同一个早停边界，explain 报的扫描量就是执行器真实读到的量。
    let stop_after = scan_stop_after(&sort, skip, limit);
    let pushed = pushed_skip(&access, &sort, skip);
    let direction = match &access {
        QueryAccess::Collection { direction: -1 } => "backward",
        QueryAccess::Index(plan) => plan.direction,
        _ => "forward",
    };
    let plan = match (&access, name) {
        (QueryAccess::Index(plan), "find") => hinted_find_plan(plan, skip, limit, &sort),
        (QueryAccess::Index(plan), _) => hinted_count_plan(plan),
        (QueryAccess::Collection { .. }, _) => winning_plan(name, &normalized, skip, limit, direction),
    };
    let optimization_millis = planning_started.elapsed().as_millis() as i32;

    // 执行耗时来自真实扫描的单调时钟，不再写 0。
    let execution_started = std::time::Instant::now();
    let outcome = scan_with_access(store, &namespace, &filter, &access, None, true, stop_after, pushed)?;
    let execution_millis = execution_started.elapsed().as_millis() as i32;
    let matched = outcome.documents.len() + outcome.skipped_at_index;
    let counted = if limit > 0 { matched.saturating_sub(skip).min(limit) } else { matched.saturating_sub(skip) };
    // `total` 取自本次执行真实读到的文档数：没有提前停止时它就是集合大小，
    // 提前停止时它反映实际工作量，不再来自一次前置全量收集。
    let total = outcome.documents_examined.max(0) as usize;

    let mut query_planner = BsonDocument::new();
    query_planner.push("namespace".to_string(), BsonValue::String(namespace.clone()));
    query_planner.push("parsedQuery".to_string(), BsonValue::Document(normalized.clone()));
    query_planner.push("indexFilterSet".to_string(), BsonValue::Bool(false));
    // 形状哈希来自规范化查询，计划键来自真实 winning plan，不再从命令原文伪造。
    let shape_seed = format!("{namespace}|{name}|{}", json::document_to_json(&normalized));
    let plan_key = stable_hash(&format!("{shape_seed}|{}", json::document_to_json(&plan)), 1);
    let query_hash = stable_hash(&shape_seed, 0);
    query_planner.push("queryHash".to_string(), BsonValue::String(query_hash.clone()));
    query_planner.push("planCacheShapeHash".to_string(), BsonValue::String(query_hash.clone()));
    query_planner.push("planCacheKey".to_string(), BsonValue::String(plan_key));
    query_planner.push("optimizationTimeMillis".to_string(), BsonValue::Int32(optimization_millis));
    query_planner.push("maxIndexedOrSolutionsReached".to_string(), BsonValue::Bool(false));
    query_planner.push("maxIndexedAndSolutionsReached".to_string(), BsonValue::Bool(false));
    query_planner.push("maxScansToExplodeReached".to_string(), BsonValue::Bool(false));
    query_planner.push("prunedSimilarIndexes".to_string(), BsonValue::Bool(false));
    query_planner.push("winningPlan".to_string(), BsonValue::Document(plan));
    query_planner.push("rejectedPlans".to_string(), BsonValue::Array(Vec::new()));

    let mut reply = BsonDocument::new();
    reply.push("explainVersion".to_string(), BsonValue::String("1".to_string()));
    reply.push("queryPlanner".to_string(), BsonValue::Document(query_planner));
    // `count` explain omits `queryShapeHash`; `find` carries it.
    if name == "find" {
        reply.push("queryShapeHash".to_string(), BsonValue::String(stable_shape_hash(&shape_seed)));
    }
    if verbosity != "queryPlanner" {
        let stats = match (&access, name) {
            (QueryAccess::Index(plan), "find") => reported_find_stats(plan, &outcome, matched, skip, limit, verbosity == "allPlansExecution"),
            (QueryAccess::Index(plan), _) => reported_count_stats(plan, &outcome, matched, skip, limit, verbosity == "allPlansExecution"),
            (QueryAccess::Collection { .. }, _) => explain_execution_stats(ExecutionStats {
                command: name,
                filter: &normalized,
                total,
                examined: outcome.documents_examined,
                matched,
                skip,
                limit,
                counted,
                all_plans: verbosity == "allPlansExecution",
                direction,
                execution_millis,
            }),
        };
        reply.push("executionStats".to_string(), BsonValue::Document(stats));
    }
    let mut echoed = inner.clone();
    if echoed.get("$db").is_none() {
        echoed.push("$db".to_string(), BsonValue::String(database));
    }
    reply.push("command".to_string(), BsonValue::Document(echoed));
    reply.push("serverInfo".to_string(), BsonValue::Document(server_info()));
    reply.push("serverParameters".to_string(), BsonValue::Document(server_parameters()));
    // `explain` replies carry `ok` as a double, unlike the plain command replies.
    reply.push("ok".to_string(), BsonValue::Double(1.0));
    Ok(reply)
}

pub(crate) fn explain_skip_limit(command: &BsonDocument) -> Result<(usize, usize), CommandError> {
    let skip = match command.get("skip") {
        Some(value) => match int_value(value) {
            Some(number) if number >= 0 => number as usize,
            Some(number) => {
                return Err(CommandError::new(2, format!("BSON field 'skip' value must be >= 0, actual value '{number}'")));
            }
            None => 0,
        },
        None => 0,
    };
    let limit = match command.get("limit") {
        Some(value) => int_value(value).map(|number| number.unsigned_abs() as usize).unwrap_or(0),
        None => 0,
    };
    Ok((skip, limit))
}

pub(crate) struct ExecutionStats<'a> {
    pub(crate) command: &'a str,
    pub(crate) filter: &'a BsonDocument,
    pub(crate) total: usize,
    /// 本次扫描实际读到的文档数：`limit` 早停后不再是整个集合。
    pub(crate) examined: i64,
    pub(crate) matched: usize,
    pub(crate) skip: usize,
    pub(crate) limit: usize,
    pub(crate) counted: usize,
    pub(crate) all_plans: bool,
    pub(crate) direction: &'a str,
    /// 本次执行真实消耗的毫秒数（单调时钟），由调用方测量后传入。
    pub(crate) execution_millis: i32,
}

pub(crate) fn explain_execution_stats(request: ExecutionStats<'_>) -> BsonDocument {
    let ExecutionStats { command, filter, total, examined, matched, skip, limit, counted, all_plans, direction, execution_millis } = request;
    let works = (total + 1) as i64;
    let returned = if command == "count" { 0 } else { counted };
    let scan = execution_stage("COLLSCAN", matched as i64, works, examined, filter, direction);
    let stage = if command == "count" {
        let mut counted_stage = BsonDocument::new();
        for (field, value) in execution_stage("COUNT", 0, works, examined, &BsonDocument::new(), direction).fields {
            counted_stage.push(field, value);
        }
        counted_stage.push("nCounted".to_string(), count_value(counted as i64));
        counted_stage.push("nSkipped".to_string(), count_value(skip as i64));
        counted_stage.push("inputStage".to_string(), BsonValue::Document(scan));
        counted_stage
    }
    else {
        let mut stage = scan;
        if skip > 0 {
            let mut skipped = execution_stage("SKIP", returned as i64, works, examined, &BsonDocument::new(), direction);
            skipped.push("skipAmount".to_string(), count_value(skip as i64));
            skipped.push("inputStage".to_string(), BsonValue::Document(stage));
            stage = skipped;
        }
        if limit > 0 {
            let mut limited = BsonDocument::new();
            for (field, value) in execution_stage("LIMIT", returned as i64, works, examined, &BsonDocument::new(), direction).fields {
                limited.push(field, value);
            }
            limited.push("limitAmount".to_string(), count_value(limit as i64));
            limited.push("inputStage".to_string(), BsonValue::Document(stage));
            stage = limited;
        }
        stage
    };
    // `isCached` marks only the outermost execution stage; nested stages omit it.
    let mut cached = BsonDocument::new();
    cached.push("isCached".to_string(), BsonValue::Bool(false));
    for (field, value) in stage.fields {
        cached.push(field, value);
    }
    let mut stage = cached;
    // 只有最外层能给出真实执行耗时；内层算子没有独立埋点，不再写 0 冒充估计值。
    stage.push("executionTimeMillisEstimate".to_string(), BsonValue::Int32(execution_millis));
    let mut stats = BsonDocument::new();
    stats.push("executionSuccess".to_string(), BsonValue::Bool(true));
    stats.push("nReturned".to_string(), count_value(returned as i64));
    stats.push("executionTimeMillis".to_string(), BsonValue::Int32(execution_millis));
    stats.push("totalKeysExamined".to_string(), BsonValue::Int32(0));
    stats.push("totalDocsExamined".to_string(), count_value(examined));
    stats.push("executionStages".to_string(), BsonValue::Document(stage));
    if all_plans {
        stats.push("allPlansExecution".to_string(), BsonValue::Array(Vec::new()));
    }
    stats
}

pub(crate) fn execution_stage(stage: &str, advanced: i64, works: i64, examined: i64, filter: &BsonDocument, direction: &str) -> BsonDocument {
    let mut document = BsonDocument::new();
    document.push("stage".to_string(), BsonValue::String(stage.to_string()));
    if !filter.fields.is_empty() {
        document.push("filter".to_string(), BsonValue::Document(filter.clone()));
    }
    document.push("nReturned".to_string(), count_value(advanced));
    document.push("works".to_string(), count_value(works));
    document.push("advanced".to_string(), count_value(advanced));
    document.push("needTime".to_string(), count_value(works - advanced - 1));
    document.push("needYield".to_string(), BsonValue::Int32(0));
    document.push("saveState".to_string(), BsonValue::Int32(0));
    document.push("restoreState".to_string(), BsonValue::Int32(0));
    document.push("isEOF".to_string(), BsonValue::Int32(1));
    if stage == "COLLSCAN" {
        document.push("direction".to_string(), BsonValue::String(direction.to_string()));
        document.push("docsExamined".to_string(), count_value(examined));
    }
    document
}

/// 服务器身份取自本实现，而不是模仿 `mongod` 的版本号。
pub(crate) fn server_info() -> BsonDocument {
    let mut info = BsonDocument::new();
    info.push("host".to_string(), BsonValue::String("127.0.0.1".to_string()));
    info.push("port".to_string(), BsonValue::Int32(0));
    info.push("version".to_string(), BsonValue::String(env!("CARGO_PKG_VERSION").to_string()));
    info.push("gitVersion".to_string(), BsonValue::String(option_env!("BOOST_BUILD_COMMIT").unwrap_or("unavailable").to_string()));
    info
}

pub(crate) fn server_parameters() -> BsonDocument {
    let mut parameters = BsonDocument::new();
    for name in [
        "internalQueryFacetBufferSizeBytes",
        "internalQueryFacetMaxOutputDocSizeBytes",
        "internalLookupStageIntermediateDocumentMaxSizeBytes",
        "internalDocumentSourceGroupMaxMemoryBytes",
        "internalQueryMaxBlockingSortMemoryUsageBytes",
    ] {
        parameters.push(name.to_string(), BsonValue::Int32(104857600));
    }
    parameters.push("internalQueryProhibitBlockingMergeOnMongoS".to_string(), BsonValue::Int32(0));
    parameters.push("internalQueryMaxAddToSetBytes".to_string(), BsonValue::Int32(104857600));
    parameters.push("internalDocumentSourceSetWindowFieldsMaxMemoryBytes".to_string(), BsonValue::Int32(104857600));
    parameters.push("internalQueryFrameworkControl".to_string(), BsonValue::String("trySbeRestricted".to_string()));
    parameters.push("internalQueryPlannerIgnoreIndexWithCollationForRegex".to_string(), BsonValue::Int32(1));
    parameters
}

pub(crate) fn stable_hash(seed: &str, round: usize) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes().iter().chain(std::iter::once(&(round as u8))) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016X}")[..8].to_string()
}

pub(crate) fn stable_shape_hash(seed: &str) -> String {
    (0..4)
        .map(|round| {
            format!("{:016X}", {
                let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
                for byte in seed.as_bytes().iter().chain(std::iter::once(&(round as u8))) {
                    hash ^= u64::from(*byte);
                    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                }
                hash
            })
        })
        .collect::<String>()
        .to_uppercase()
}
