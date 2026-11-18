//! 读命令：`find` / `count` / `distinct` 与它们的扫描、投影和统计装配。

use super::*;

pub(crate) fn execution_base(stage: &str, advanced: i64, works: i64, eof: bool) -> BsonDocument {
    let mut document = BsonDocument::new();
    document.push("stage".to_string(), BsonValue::String(stage.to_string()));
    document.push("nReturned".to_string(), count_value(advanced));
    document.push("executionTimeMillisEstimate".to_string(), BsonValue::Int32(0));
    document.push("works".to_string(), count_value(works));
    document.push("advanced".to_string(), count_value(advanced));
    document.push("needTime".to_string(), count_value(if eof { (works - advanced - 1).max(0) } else { 0 }));
    document.push("needYield".to_string(), BsonValue::Int32(0));
    document.push("saveState".to_string(), BsonValue::Int32(0));
    document.push("restoreState".to_string(), BsonValue::Int32(0));
    document.push("isEOF".to_string(), BsonValue::Int32(if eof { 1 } else { 0 }));
    document
}

/// 用执行器真实消费的账目渲染 `IXSCAN`。
///
/// `keys_examined` 与实际扫描的索引项数一致，`advanced` 是交付给上层的命中数
/// （含在索引层被 `skip` 跳过的项）；早停时不计读到区间末尾的那次额外 work。
pub(crate) fn index_scan_stage(plan: &HintedPlan, keys_examined: i64, advanced: i64, exhausted: bool) -> BsonDocument {
    let works = keys_examined + i64::from(exhausted);
    let mut stage = execution_base("IXSCAN", advanced, works, exhausted);
    for (field, value) in scan_stage_document(plan, false).fields.into_iter().skip(1) {
        stage.push(field, value);
    }
    stage.push("keysExamined".to_string(), count_value(keys_examined));
    stage.push("seeks".to_string(), BsonValue::Int32(if keys_examined > 0 { 1 } else { 0 }));
    stage.push("dupsTested".to_string(), BsonValue::Int32(0));
    stage.push("dupsDropped".to_string(), BsonValue::Int32(0));
    stage
}

/// 用执行器真实消费的账目渲染 `COUNT_SCAN`。
///
/// 计数不回表；`keysExamined` 连同收尾探针一并计入，与 `mongod` 数索引项的
/// 账目一致。
pub(crate) fn count_scan_stage(plan: &HintedPlan, keys_examined: i64, advanced: i64, exhausted: bool) -> BsonDocument {
    let works = keys_examined + i64::from(exhausted);
    let mut stage = execution_base("COUNT_SCAN", advanced, works, exhausted);
    for (field, value) in scan_stage_document(plan, true).fields.into_iter().skip(1) {
        stage.push(field, value);
    }
    stage.push("keysExamined".to_string(), count_value(works));
    stage
}

pub(crate) fn reported_find_stats(
    plan: &HintedPlan,
    outcome: &ScanOutcome,
    matched: usize,
    skip: usize,
    limit: usize,
    all_plans: bool,
) -> BsonDocument {
    let keys_examined = outcome.keys_examined;
    let documents_examined = outcome.documents_examined;
    let skipped = outcome.skipped_at_index as i64;
    let works = keys_examined + i64::from(outcome.exhausted);
    let mut stage = index_scan_stage(plan, keys_examined, documents_examined + skipped, outcome.exhausted);
    if skipped > 0 {
        // `skip` 下推到索引层：`SKIP` 夹在 `IXSCAN` 与 `FETCH` 之间，跳过的命中不回表。
        let mut skipped_stage = execution_base("SKIP", documents_examined, works, outcome.exhausted);
        skipped_stage.push("skipAmount".to_string(), count_value(skip as i64));
        skipped_stage.push("inputStage".to_string(), BsonValue::Document(stage));
        stage = skipped_stage;
    }
    let mut fetch = execution_base("FETCH", documents_examined, works, outcome.exhausted);
    if !plan.residual.fields.is_empty() {
        fetch.push("filter".to_string(), BsonValue::Document(plan.residual.clone()));
    }
    fetch.push("docsExamined".to_string(), count_value(documents_examined));
    fetch.push("alreadyHasObj".to_string(), BsonValue::Int32(0));
    fetch.push("inputStage".to_string(), BsonValue::Document(stage));
    stage = fetch;
    if !plan.sort_covered {
        let mut sorted = execution_base("SORT", matched as i64, keys_examined + 1, true);
        sorted.push("inputStage".to_string(), BsonValue::Document(stage));
        stage = sorted;
    }
    let after_skip = matched.saturating_sub(skip);
    let returned = if limit > 0 { after_skip.min(limit) } else { after_skip };
    if skip > 0 && skipped == 0 {
        let mut skipped_stage = execution_base("SKIP", after_skip as i64, keys_examined + 1, true);
        skipped_stage.push("skipAmount".to_string(), count_value(skip as i64));
        skipped_stage.push("inputStage".to_string(), BsonValue::Document(stage));
        stage = skipped_stage;
    }
    if limit > 0 {
        let mut limited = execution_base("LIMIT", returned as i64, keys_examined + 1, true);
        limited.push("limitAmount".to_string(), count_value(limit as i64));
        limited.push("inputStage".to_string(), BsonValue::Document(stage));
        stage = limited;
    }
    let mut cached = BsonDocument::new();
    cached.push("isCached".to_string(), BsonValue::Bool(false));
    for (field, value) in stage.fields {
        cached.push(field, value);
    }
    let mut stats = BsonDocument::new();
    stats.push("executionSuccess".to_string(), BsonValue::Bool(true));
    stats.push("nReturned".to_string(), count_value(returned as i64));
    stats.push("executionTimeMillis".to_string(), BsonValue::Int32(0));
    stats.push("totalKeysExamined".to_string(), count_value(keys_examined));
    stats.push("totalDocsExamined".to_string(), count_value(documents_examined));
    stats.push("executionStages".to_string(), BsonValue::Document(cached));
    if all_plans {
        stats.push("allPlansExecution".to_string(), BsonValue::Array(Vec::new()));
    }
    stats
}

pub(crate) fn reported_count_stats(
    plan: &HintedPlan,
    outcome: &ScanOutcome,
    matched: usize,
    skip: usize,
    limit: usize,
    all_plans: bool,
) -> BsonDocument {
    let keys_examined = outcome.keys_examined;
    let documents_examined = outcome.documents_examined;
    let advanced = documents_examined + outcome.skipped_at_index as i64;
    let exhausted = outcome.exhausted;
    let index_only = plan.residual.fields.is_empty();
    let input = if index_only {
        count_scan_stage(plan, keys_examined, advanced, exhausted)
    }
    else {
        let works = keys_examined + i64::from(exhausted);
        let stage = index_scan_stage(plan, keys_examined, advanced, exhausted);
        let mut fetch = execution_base("FETCH", advanced, works, exhausted);
        fetch.push("filter".to_string(), BsonValue::Document(plan.residual.clone()));
        fetch.push("docsExamined".to_string(), count_value(documents_examined));
        fetch.push("alreadyHasObj".to_string(), BsonValue::Int32(0));
        fetch.push("inputStage".to_string(), BsonValue::Document(stage));
        fetch
    };
    let skipped_matches = matched.min(skip);
    let available = matched - skipped_matches;
    let counted = if limit > 0 { available.min(limit) } else { available };
    let mut counted_stage = execution_base("COUNT", 0, keys_examined + 1, true);
    counted_stage.push("nCounted".to_string(), count_value(counted as i64));
    counted_stage.push("nSkipped".to_string(), count_value(skipped_matches as i64));
    counted_stage.push("inputStage".to_string(), BsonValue::Document(input));
    let mut cached = BsonDocument::new();
    cached.push("isCached".to_string(), BsonValue::Bool(false));
    for (field, value) in counted_stage.fields {
        cached.push(field, value);
    }
    let mut stats = BsonDocument::new();
    stats.push("executionSuccess".to_string(), BsonValue::Bool(true));
    stats.push("nReturned".to_string(), BsonValue::Int32(0));
    stats.push("executionTimeMillis".to_string(), BsonValue::Int32(0));
    stats.push("totalKeysExamined".to_string(), count_value(keys_examined + i64::from(exhausted && index_only)));
    stats.push("totalDocsExamined".to_string(), count_value(if index_only { 0 } else { documents_examined }));
    stats.push("executionStages".to_string(), BsonValue::Document(cached));
    if all_plans {
        stats.push("allPlansExecution".to_string(), BsonValue::Array(Vec::new()));
    }
    stats
}

/// 执行器真实使用的访问路径。
///
/// `find`、`count`、`explain` 都先调用同一个 [`choose_access`]，因此
/// `explain` 报告的结构就是执行器实际消费的结构，不存在「计划说 IXSCAN、
/// 实际全表扫描」的第二份判断。
pub(crate) enum QueryAccess {
    /// 按编码 `_id` 顺序扫描整个集合，`direction` 为自然序方向。
    Collection { direction: i32 },
    /// 按索引项前缀探针读取候选文档，再回表复核完整过滤条件。
    Index(HintedPlan),
}

/// 一次扫描的真实账目。
pub(crate) struct ScanOutcome {
    pub(crate) documents: Vec<BsonDocument>,
    pub(crate) keys_examined: i64,
    pub(crate) documents_examined: i64,
    /// 扫描是否读到区间末尾；`limit` 早停后为 `false`。
    pub(crate) exhausted: bool,
    /// 在索引层被跳过、没有回表的命中数；只有边界完整消费查询时才可能非零。
    pub(crate) skipped_at_index: usize,
}

/// 边界完整消费查询时，`skip` 可以下推到索引层：跳过的命中不回表。
///
/// 只有结果顺序就是扫描顺序（无排序，或排序由索引序覆盖）时才允许下推，
/// 否则「先跳」与「先排」会给出不同的窗口。
pub(crate) fn pushed_skip(access: &QueryAccess, sort: &[(String, i32)], skip: usize) -> usize {
    match access {
        QueryAccess::Index(plan) if plan.residual.fields.is_empty() && (sort.is_empty() || plan.sort_covered) => skip,
        _ => 0,
    }
}

/// 决定一次查询走集合扫描还是索引探针。
///
/// 事务内的写入只暂存在文档命名空间，索引项在提交前不对外可见，因此事务内
/// 一律走集合扫描，保证读到会话自己的未提交写入。
pub(crate) fn choose_access(
    store: &dyn MongoStorage,
    namespace: &str,
    filter: &BsonDocument,
    hint: &Hint,
    sort: &[(String, i32)],
    documents: &[BsonDocument],
    session: Option<&str>,
) -> Result<QueryAccess, CommandError> {
    if session.is_some() {
        return Ok(QueryAccess::Collection { direction: 1 });
    }
    match hint {
        Hint::Natural(direction) => Ok(QueryAccess::Collection { direction: *direction }),
        Hint::Index(specification) => {
            let plan = build_hinted_plan(&multi_key_fields(documents, specification), specification, filter, sort, &AggregationExpressions);
            if index_executable(&plan, filter, true, &AggregationExpressions) && index_has_entries(store, namespace, specification)? {
                Ok(QueryAccess::Index(plan))
            }
            else {
                Ok(QueryAccess::Collection { direction: 1 })
            }
        }
        Hint::None => match automatic_index(store, namespace, filter)? {
            Some(specification) => Ok(QueryAccess::Index(build_hinted_plan(
                &multi_key_fields(documents, &specification),
                &specification,
                filter,
                sort,
                &AggregationExpressions,
            ))),
            None => Ok(QueryAccess::Collection { direction: 1 }),
        },
    }
}

/// 按访问路径执行一次真实扫描。
///
/// `stop_after` 是命中数边界：达到之后立即停止读取，用于 `limit` 早停。只有
/// 不受排序约束的查询才能给出这个边界，否则「先停」与「先排」会给出不同结果。
/// `skip` 是索引层跳过的命中数（仅在边界完整消费查询时下推），跳过的命中不
/// 回表，账目与 `mongod` 的 `FETCH{SKIP{IXSCAN}}` 一致。反向扫描读完整个区间
/// 再倒序交付，索引项读取数如实计入。
#[allow(clippy::too_many_arguments)]
pub(crate) fn scan_with_access(
    store: &dyn MongoStorage,
    namespace: &str,
    filter: &BsonDocument,
    access: &QueryAccess,
    session: Option<&str>,
    need_total: bool,
    stop_after: Option<usize>,
    skip: usize,
) -> Result<ScanOutcome, CommandError> {
    match access {
        QueryAccess::Collection { .. } => {
            let (documents, documents_examined) = collect_matching_bounded(store, namespace, filter, session, need_total, stop_after)?;
            Ok(ScanOutcome { documents, keys_examined: 0, documents_examined, exhausted: true, skipped_at_index: 0 })
        }
        QueryAccess::Index(plan) => {
            let entry_namespace = crate::mongo::storage::index_namespace(namespace, &plan.spec.name);
            let spans = plan_spans(plan).unwrap_or_else(|| vec![IndexSpan { prefix: Vec::new(), after: None, before: None }]);
            let backward = plan.direction == "backward";
            let mut keys_examined = 0i64;
            let mut documents_examined = 0i64;
            let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
            let mut documents = Vec::new();
            let mut skipped_at_index = 0usize;
            let mut exhausted = true;
            let deliver = |document_key: Vec<u8>,
                           store: &dyn MongoStorage,
                           seen: &mut BTreeSet<Vec<u8>>,
                           documents: &mut Vec<BsonDocument>,
                           documents_examined: &mut i64,
                           skipped_at_index: &mut usize|
             -> Result<bool, CommandError> {
                if !seen.insert(document_key.clone()) {
                    return Ok(false);
                }
                if *skipped_at_index < skip {
                    // `skip` 下推到索引层：跳过的命中不回表，与 `mongod` 的 `FETCH{SKIP{IXSCAN}}` 账目一致。
                    *skipped_at_index += 1;
                    return Ok(true);
                }
                // 回表尝试如实计数：悬挂索引项也算一次 `docsExamined`，与 `mongod` 一致。
                *documents_examined += 1;
                if let Some(payload) = namespace_document(store, namespace, &document_key, session)?
                    && let Ok(document) = parse_bson(&payload)
                    && document_matches(&document, filter)?
                {
                    documents.push(document);
                    return Ok(true);
                }
                Ok(false)
            };
            'scan: for span in spans {
                let mut cursor = span.after.clone();
                if backward {
                    // 反向扫描没有按游标倒读的能力：整段读完再倒序交付，索引项读取数如实计入。
                    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
                    loop {
                        let batch = store
                            .scan_reserved_range(&entry_namespace, &span.prefix, cursor.as_deref(), span.before.as_deref(), 0)
                            .map_err(|error| CommandError::new(1, format!("index scan failed: {error}")))?;
                        if batch.is_empty() {
                            break;
                        }
                        entries.extend(batch);
                        if let Some(last) = entries.last() {
                            cursor = Some(last.0.clone());
                        }
                    }
                    keys_examined += entries.len() as i64;
                    entries.reverse();
                    for (_entry_key, document_key) in entries {
                        deliver(document_key, store, &mut seen, &mut documents, &mut documents_examined, &mut skipped_at_index)?;
                    }
                    continue 'scan;
                }
                // 区间内部分页：每页只读还差的命中数，避免为了早停把整段前缀取回。
                loop {
                    let page = match stop_after {
                        Some(bound) => (bound - documents.len() - skipped_at_index).max(1),
                        None => 0,
                    };
                    let batch = store
                        .scan_reserved_range(&entry_namespace, &span.prefix, cursor.as_deref(), span.before.as_deref(), page)
                        .map_err(|error| CommandError::new(1, format!("index scan failed: {error}")))?;
                    if batch.is_empty() {
                        break;
                    }
                    let fetched = batch.len();
                    keys_examined += fetched as i64;
                    for (entry_key, document_key) in batch {
                        cursor = Some(entry_key);
                        if deliver(document_key, store, &mut seen, &mut documents, &mut documents_examined, &mut skipped_at_index)?
                            && stop_after.is_some_and(|bound| documents.len() + skipped_at_index >= bound)
                        {
                            exhausted = false;
                            break 'scan;
                        }
                    }
                    if stop_after.is_none() || fetched < page {
                        break;
                    }
                }
            }
            Ok(ScanOutcome { documents, keys_examined, documents_examined, exhausted, skipped_at_index })
        }
    }
}

/// 只读前 `skip + limit` 篇命中即可结束扫描的边界。
///
/// 排序非空时结果顺序由排序决定，不能提前停；`limit` 为 `0` 表示不限量。
pub(crate) fn scan_stop_after(sort: &[(String, i32)], skip: usize, limit: usize) -> Option<usize> {
    if sort.is_empty() && limit > 0 { Some(skip.saturating_add(limit)) } else { None }
}

/// `find` with filter, sort, skip, limit and cursor batching.
pub fn find(command: &BsonDocument, store: &dyn MongoStorage, cursors: &mut CursorStore) -> Result<BsonDocument, CommandError> {
    let namespace = namespace_from_command(command, "find").ok_or_else(|| CommandError::new(73, "find command missing collection name"))?;
    let session = SessionContext::new(command, store)?;

    let filter = match command.get("filter") {
        Some(BsonValue::Document(filter)) => filter.clone(),
        _ => BsonDocument::new(),
    };
    validate_filter(&filter, "find")?;
    let sort = parse_sort(command.get("sort"));
    let skip = parse_count(command.get("skip"));
    let limit = parse_count(command.get("limit"));
    // `None` keeps the absence of `batchSize`, which means the default 101
    // documents; an explicit `0` opens a cursor with an empty first batch.
    let batch_size = command.get("batchSize").and_then(int_value).map(|count| count.max(0) as usize);
    let projection = match command.get("projection") {
        Some(BsonValue::Document(projection)) => Some(projection.clone()),
        _ => None,
    };
    let hint = resolve_hint(command, store, &namespace, &filter, command.get("sort"), command.get("projection"))?;

    let access = choose_access(store, &namespace, &filter, &hint, &sort, &[], session.key())?;
    // 无排序、无 skip/limit 的普通集合扫描可以推迟到 `getMore`：首包只读一页，
    // 续扫位置留在游标里，避免一开始就把整个结果集物化。索引扫描、反向扫描与
    // 事务视图都不能按 `_id` 游标续读，仍走整段物化。
    let deferred = sort.is_empty()
        && skip == 0
        && limit == 0
        && session.key().is_none()
        && matches!(access, QueryAccess::Collection { direction: 1 })
        && store.supports_page_scan();
    if deferred {
        // 首包必须是 `batchSize`（缺省 101）篇命中，而不是第一个扫描页里的命中；
        // 谓词选择性强时一页不足，继续按页补齐，游标位置停在已发出文档之后。
        let want = batch_size.unwrap_or(DEFAULT_FIRST_BATCH);
        let mut values: Vec<BsonValue> = Vec::new();
        let mut after = None;
        loop {
            let (documents, next) = scan_collection_page(store, &namespace, &filter, SCAN_PAGE, after.as_deref())?;
            for document in documents {
                if let Some(value) = project_document(document, projection.as_ref())? {
                    values.push(value);
                }
            }
            after = next;
            if values.len() >= want || after.is_none() {
                break;
            }
        }
        let scan = after.map(|after| CursorScan { filter: filter.clone(), after: Some(after), projection: projection.clone() });
        let (cursor_id, first_batch) = cursors.open_scan(&namespace, values, batch_size, scan);
        return Ok(cursor_reply(&namespace, "firstBatch", first_batch, cursor_id));
    }
    // 无排序时结果顺序就是扫描顺序，命中前 `skip + limit` 篇即可停手。
    let stop_after = scan_stop_after(&sort, skip, limit);
    let pushed = pushed_skip(&access, &sort, skip);
    let mut documents = scan_with_access(store, &namespace, &filter, &access, session.key(), false, stop_after, pushed)?.documents;
    if matches!(access, QueryAccess::Collection { direction: -1 }) {
        documents.reverse();
    }
    sort_documents(&mut documents, &sort);

    let start = if pushed > 0 { 0 } else { skip.min(documents.len()) };
    let end = if limit > 0 { (start + limit).min(documents.len()) } else { documents.len() };
    let mut window: Vec<BsonValue> = Vec::new();
    for document in documents.into_iter().skip(start).take(end - start) {
        match &projection {
            Some(projection) => {
                if let Some(projected) = project_stage(&[document], projection)?.into_iter().next() {
                    window.push(BsonValue::Document(projected));
                }
            }
            None => window.push(BsonValue::Document(document)),
        }
    }

    let (cursor_id, first_batch) = cursors.open(&namespace, window, batch_size);
    Ok(cursor_reply(&namespace, "firstBatch", first_batch, cursor_id))
}

/// `count`, honouring `query`, `skip` and `limit`.
pub fn count(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let namespace = namespace_from_command(command, "count").ok_or_else(|| CommandError::new(73, "count command missing collection name"))?;
    let session = SessionContext::new(command, store)?;

    let filter = match command.get("query") {
        Some(BsonValue::Document(filter)) => filter.clone(),
        _ => BsonDocument::new(),
    };
    validate_filter(&filter, "count")?;
    let hint = resolve_hint(command, store, &namespace, &filter, None, None)?;
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
    // A negative `limit` counts backwards from the end of the match set, which
    // is the same window size as its absolute value.
    let limit = match command.get("limit") {
        Some(value) => int_value(value).map(|number| number.unsigned_abs() as usize).unwrap_or(0),
        None => 0,
    };

    // 空查询、无 skip/limit、未指定 hint 时，`count` 就是命名空间的文档数：
    // 不必把每篇文档解码成 `BsonDocument`，直接按事务视图或已提交数据取条数。
    let matched = if filter.fields.is_empty() && skip == 0 && limit == 0 && command.get("hint").is_none() {
        namespace_documents(store, &namespace, session.key())?.len()
    }
    else {
        let access = choose_access(store, &namespace, &filter, &hint, &[], &[], session.key())?;
        let stop_after = scan_stop_after(&[], skip, limit);
        let pushed = pushed_skip(&access, &[], skip);
        match &access {
            // 非空 filter 的集合扫描只需要谓词字段；能静态收敛时按投影解码，
            // 并用同一早停边界在命中足够后立即停止读取。
            QueryAccess::Collection { direction: 1 } => {
                let fields = filter_projection_fields(&filter);
                let mut matched = 0_usize;
                scan_namespace_pages(store, &namespace, session.key(), |value| {
                    let decoded = match fields.as_deref() {
                        Some(fields) => parse_bson_projected(value, fields),
                        None => parse_bson(value),
                    };
                    if let Ok(candidate) = decoded
                        && document_matches(&candidate, &filter)?
                    {
                        matched += 1;
                        if stop_after.is_some_and(|bound| matched >= bound) {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                })?;
                matched
            }
            _ => {
                let outcome = scan_with_access(store, &namespace, &filter, &access, session.key(), false, stop_after, pushed)?;
                outcome.documents.len() + outcome.skipped_at_index
            }
        }
    };
    let start = skip.min(matched);
    let end = if limit > 0 { (start + limit).min(matched) } else { matched };

    Ok(write_reply((end - start) as i64, None, &[]))
}

/// `distinct`: nested paths, one-level array flattening, query filtering, and
/// first-occurrence ordering.
pub fn distinct(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let namespace =
        namespace_from_command(command, "distinct").ok_or_else(|| CommandError::new(73, "distinct command missing collection name"))?;
    let session = SessionContext::new(command, store)?;
    let Some(key) = command.get("key")
    else {
        return Err(CommandError::new(40414, "BSON field 'distinct.key' is missing but a required field"));
    };
    let BsonValue::String(key) = key
    else {
        return Err(CommandError::new(
            14,
            format!("BSON field 'distinctCommandRequest.key' is the wrong type '{}', expected type 'string'", bson_type_name(key)),
        ));
    };
    let empty = BsonDocument::new();
    let filter = match command.get("query") {
        Some(BsonValue::Document(filter)) => filter,
        _ => &empty,
    };
    validate_filter(filter, "distinct")?;

    let segments: Vec<&str> = key.split('.').collect();
    let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut values = Vec::new();
    // `distinct` 只读谓词字段与取值路径的首段；能静态收敛时跳过其余字段。
    let mut decode_fields = filter_projection_fields(filter);
    if let Some(fields) = decode_fields.as_mut() {
        let top = segments.first().copied().unwrap_or("");
        if !fields.iter().any(|field| field == top) {
            fields.push(top.to_string());
        }
    }
    scan_namespace_pages(store, &namespace, session.key(), |payload| {
        let decoded = match decode_fields.as_deref() {
            Some(fields) => parse_bson_projected(payload, fields),
            None => parse_bson(payload),
        };
        let Ok(document) = decoded
        else {
            return Ok(true);
        };
        if !document_matches(&document, filter)? {
            return Ok(true);
        }
        let mut matched = Vec::new();
        collect_values_at(&document, &segments, &mut matched);
        for value in matched {
            collect_distinct(value, &mut seen, &mut values);
        }
        Ok(true)
    })?;
    // `mongod` collects distinct values in a sorted BSON set, so the reply is
    // ordered by canonical type and value instead of by scan order.
    values.sort_by(compare_values);
    let mut response = BsonDocument::new();
    response.push("values".to_string(), BsonValue::Array(values));
    response.push("ok".to_string(), BsonValue::Double(1.0));
    Ok(response)
}

pub(crate) fn collect_distinct(value: &BsonValue, seen: &mut BTreeSet<Vec<u8>>, values: &mut Vec<BsonValue>) {
    match value {
        BsonValue::Array(items) => {
            for item in items {
                if seen.insert(value_to_key(item)) {
                    values.push(item.clone());
                }
            }
        }
        other => {
            if seen.insert(value_to_key(other)) {
                values.push(other.clone());
            }
        }
    }
}

/// 套用 `find` 的投影；被投影排除的文档直接丢弃，与窗口语义一致。
pub(crate) fn project_document(document: BsonDocument, projection: Option<&BsonDocument>) -> Result<Option<BsonValue>, CommandError> {
    match projection {
        Some(projection) => Ok(project_stage(&[document], projection)?.into_iter().next().map(BsonValue::Document)),
        None => Ok(Some(BsonValue::Document(document))),
    }
}

/// 过滤谓词只读取的顶层字段；无法安全收敛时返回 `None`（退回整篇解码）。
///
/// `count` / `distinct` 不需要把每篇文档都建成完整 `BsonDocument`：谓词引用
/// 的顶层字段之外，其余字段按线协议长度跳过即可。`$where`、`$expr` 一类可以
/// 读整篇文档的算子无法静态分析，遇到就放弃投影。
pub(crate) fn filter_projection_fields(filter: &BsonDocument) -> Option<Vec<String>> {
    fn collect(document: &BsonDocument, fields: &mut BTreeSet<String>) -> bool {
        for (name, value) in &document.fields {
            match name.as_str() {
                "$and" | "$or" | "$nor" => {
                    let BsonValue::Array(items) = value
                    else {
                        return false;
                    };
                    for item in items {
                        let BsonValue::Document(item) = item
                        else {
                            return false;
                        };
                        if !collect(item, fields) {
                            return false;
                        }
                    }
                }
                "$where" | "$expr" | "$text" | "$jsonSchema" | "$comment" | "$near" | "$nearSphere" => return false,
                _ if name.starts_with('$') => return false,
                _ => {
                    fields.insert(name.split('.').next().unwrap_or(name).to_string());
                }
            }
        }
        true
    }
    let mut fields = BTreeSet::new();
    collect(filter, &mut fields).then(|| fields.into_iter().collect())
}

/// 按页遍历命名空间，`visit` 返回 `false` 时提前结束。
///
/// 事务内没有本命名空间暂存写入时按已提交数据分页；只有合并视图无法按游标
/// 推进时才整段读取。与 `find` 的早停扫描共用同一页大小。
pub(crate) fn scan_namespace_pages<F>(
    store: &dyn MongoStorage,
    namespace: &str,
    session: Option<&str>,
    mut visit: F,
) -> Result<(), CommandError>
where
    F: FnMut(&[u8]) -> Result<bool, CommandError>,
{
    if let Some(session) = session {
        let mut after: Option<Vec<u8>> = None;
        loop {
            let page = store
                .transaction_scan_page(session, namespace, after.as_deref(), SCAN_PAGE)
                .map_err(|error| CommandError::new(1, format!("scan failed: {error}")))?;
            let documents = match page {
                TransactionPage::None => store
                    .scan_page(namespace, after.as_deref(), SCAN_PAGE)
                    .map_err(|error| CommandError::new(1, format!("scan failed: {error}")))?,
                TransactionPage::Page(page) => page,
                TransactionPage::Merged => {
                    let documents = store
                        .transaction_scan(session, namespace)
                        .map_err(|error| CommandError::new(1, format!("scan failed: {error}")))?
                        .unwrap_or_default();
                    for (_key, value) in documents {
                        if !visit(&value)? {
                            return Ok(());
                        }
                    }
                    return Ok(());
                }
            };
            let fetched = documents.len();
            after = documents.last().map(|(key, _)| key.clone());
            for (_key, value) in documents {
                if !visit(&value)? {
                    return Ok(());
                }
            }
            if fetched < SCAN_PAGE {
                break;
            }
        }
    }
    else {
        let mut after: Option<Vec<u8>> = None;
        loop {
            let page = store
                .scan_page(namespace, after.as_deref(), SCAN_PAGE)
                .map_err(|error| CommandError::new(1, format!("scan failed: {error}")))?;
            let fetched = page.len();
            after = page.last().map(|(key, _)| key.clone());
            for (_key, value) in page {
                if !visit(&value)? {
                    return Ok(());
                }
            }
            if fetched < SCAN_PAGE {
                break;
            }
        }
    }
    Ok(())
}

/// 与 [`collect_matching`] 相同，但命中数达到 `stop_after` 后停止读取。
///
/// 第二个返回值是实际读到的文档数，供 `explain` 报告 `docsExamined`：提前停
/// 之后不能再报整个集合的大小。`need_total` 为假时调用方不看这个数字。
pub(crate) fn collect_matching_bounded(
    store: &dyn MongoStorage,
    namespace: &str,
    filter: &BsonDocument,
    session: Option<&str>,
    need_total: bool,
    stop_after: Option<usize>,
) -> Result<(Vec<BsonDocument>, i64), CommandError> {
    if stop_after.is_none() {
        let documents = collect_matching(store, namespace, filter, session)?;
        let total = if need_total { namespace_documents(store, namespace, session)?.len() as i64 } else { 0 };
        return Ok((documents, total));
    }
    if let Some(identifier) = single_id_lookup(filter) {
        let key = value_to_key(identifier);
        return Ok(match namespace_document(store, namespace, &key, session)? {
            Some(bytes) => match parse_bson(&bytes) {
                Ok(document) if document_matches(&document, filter)? => (vec![document], 1),
                _ => (Vec::new(), 1),
            },
            None => (Vec::new(), 0),
        });
    }
    let mut documents = Vec::new();
    let mut examined = 0i64;
    if let Some(session) = session {
        // 只读事务（或没写过本命名空间的事务）直接按已提交数据分页；只有暂存
        // 写入存在时合并视图才无法按游标推进，此时退回整段读。
        let mut after: Option<Vec<u8>> = None;
        loop {
            let page = store
                .transaction_scan_page(session, namespace, after.as_deref(), SCAN_PAGE)
                .map_err(|error| CommandError::new(1, format!("scan failed: {error}")))?;
            match page {
                TransactionPage::None => {
                    let committed = store
                        .scan_page(namespace, after.as_deref(), SCAN_PAGE)
                        .map_err(|error| CommandError::new(1, format!("scan failed: {error}")))?;
                    after = committed.last().map(|(key, _)| key.clone());
                    for (_key, value) in committed {
                        examined += 1;
                        if let Ok(candidate) = parse_bson(&value)
                            && document_matches(&candidate, filter)?
                        {
                            documents.push(candidate);
                            if stop_after.is_some_and(|bound| documents.len() >= bound) {
                                return Ok((documents, examined));
                            }
                        }
                    }
                    if after.is_none() {
                        break;
                    }
                }
                TransactionPage::Page(page) => {
                    if page.is_empty() {
                        break;
                    }
                    after = page.last().map(|(key, _)| key.clone());
                    for (_key, value) in page {
                        examined += 1;
                        if let Ok(candidate) = parse_bson(&value)
                            && document_matches(&candidate, filter)?
                        {
                            documents.push(candidate);
                            if stop_after.is_some_and(|bound| documents.len() >= bound) {
                                return Ok((documents, examined));
                            }
                        }
                    }
                }
                TransactionPage::Merged => {
                    for (_key, value) in namespace_documents(store, namespace, Some(session))? {
                        examined += 1;
                        if let Ok(candidate) = parse_bson(&value)
                            && document_matches(&candidate, filter)?
                        {
                            documents.push(candidate);
                            if stop_after.is_some_and(|bound| documents.len() >= bound) {
                                return Ok((documents, examined));
                            }
                        }
                    }
                    return Ok((documents, examined));
                }
            }
        }
        return Ok((documents, examined));
    }
    // 无排序的查询一旦凑够 `stop_after` 就不再需要后面的文档，因此按页扫描：
    // 命中前只克隆已读到的那一页，而不是先整集合取回再丢弃。
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page =
            store.scan_page(namespace, after.as_deref(), SCAN_PAGE).map_err(|error| CommandError::new(1, format!("scan failed: {error}")))?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|(key, _)| key.clone());
        for (_key, value) in page {
            examined += 1;
            if let Ok(candidate) = parse_bson(&value)
                && document_matches(&candidate, filter)?
            {
                documents.push(candidate);
                if stop_after.is_some_and(|bound| documents.len() >= bound) {
                    return Ok((documents, examined));
                }
            }
        }
    }
    Ok((documents, examined))
}
