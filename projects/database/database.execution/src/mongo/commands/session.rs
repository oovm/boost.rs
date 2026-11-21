//! 会话协作：逻辑会话与事务命令（`commitTransaction` / `abortTransaction`）与游标命令（`getMore` / `killCursors`）。

use super::*;

/// `getMore`: drains the caller-owned cursor table.
///
/// A cursor is bound to the namespace it was opened on. `mongod` requires the
/// `collection` field and refuses a request that names another namespace, so the
/// lookup is scoped before any document is read.
pub fn get_more(command: &BsonDocument, store: &dyn MongoStorage, cursors: &mut CursorStore) -> Result<BsonDocument, CommandError> {
    let Some(cursor_id) = command.get("getMore").and_then(int_value)
    else {
        return Err(CommandError::new(2, "getMore requires a cursor id"));
    };
    let Some(collection) = command.get("collection").and_then(|value| match value {
        BsonValue::String(collection) => Some(collection.as_str()),
        _ => None,
    })
    else {
        return Err(CommandError::new(40415, "BSON field 'getMore.collection' is missing but a required field"));
    };
    let namespace = format!("{}.{collection}", command.database());
    // Absent and explicit `0` both mean "fill the batch", which `mongod 8.0.9`
    // answers by draining everything that fits under the BSON ceiling.
    let batch_size = command.get("batchSize").and_then(int_value).filter(|count| *count > 0).map(|count| count as usize);
    let (batch, namespace) = cursors
        .get_more(cursor_id, &namespace, batch_size, |scan, want| {
            let mut picked: Vec<BsonValue> = Vec::new();
            loop {
                let (documents, after) =
                    scan_collection_page(store, &namespace, &scan.filter, SCAN_PAGE, scan.after.as_deref()).map_err(|error| error.message)?;
                for document in documents {
                    if let Some(value) = project_document(document, scan.projection.as_ref()).map_err(|error| error.message)? {
                        picked.push(value);
                    }
                }
                scan.after = after;
                if picked.len() >= want || scan.after.is_none() {
                    break;
                }
            }
            Ok(picked)
        })
        .map_err(|error| match error {
            GetMoreError::NotFound(id) => CommandError::new(43, format!("cursor id {id} not found")),
            // `mongod` refuses a cross-namespace request with `Unauthorized` before
            // it looks at the cursor's state.
            GetMoreError::NamespaceMismatch { requested, cursor } => CommandError::new(
                13,
                format!("Requested getMore on namespace '{requested}', but cursor belongs to a different namespace {cursor}"),
            ),
            // `mongod 8.0.9` kills the cursor with the collection and answers one
            // `QueryPlanKilled` naming the collection UUID.
            GetMoreError::CollectionDropped { namespace, .. } => {
                CommandError::new(175, format!("collection dropped. UUID {}", collection_uuid_text(&namespace)))
            }
            GetMoreError::Backend(message) => CommandError::new(1, format!("scan failed: {message}")),
        })?;
    let remaining = if cursors.contains(cursor_id) { cursor_id } else { 0 };
    Ok(cursor_reply(&namespace, "nextBatch", batch, remaining))
}

/// `killCursors`.
pub fn kill_cursors(command: &BsonDocument, cursors: &mut CursorStore) -> Result<BsonDocument, CommandError> {
    let identifiers = match command.get("cursors") {
        Some(BsonValue::Array(identifiers)) => identifiers.clone(),
        _ => Vec::new(),
    };
    let requested: Vec<i64> = identifiers.iter().filter_map(int_value).collect();
    let (killed, not_found) = cursors.kill(&requested);

    let mut response = BsonDocument::new();
    response.push("cursorsKilled".to_string(), BsonValue::Array(killed.into_iter().map(BsonValue::Int64).collect()));
    response.push("cursorsNotFound".to_string(), BsonValue::Array(not_found.into_iter().map(BsonValue::Int64).collect()));
    response.push("cursorsAlive".to_string(), BsonValue::Array(Vec::new()));
    response.push("cursorsUnknown".to_string(), BsonValue::Array(Vec::new()));
    response.push("ok".to_string(), BsonValue::Double(1.0));
    Ok(response)
}

/// `mongod 8.0.9` refuses any `writeConcern` field on a statement of a
/// multi-document transaction (`InvalidOptions`) and leaves the transaction
/// unopened. Callers run this before [`SessionContext::new`] so a rejected
/// statement cannot start a transaction that a later commit would publish.
pub(crate) fn reject_transaction_write_concern(command: &BsonDocument) -> Result<(), CommandError> {
    if command.get("writeConcern").is_none() {
        return Ok(());
    }
    let in_transaction =
        transaction_key(command).is_some() && transaction_number(command)?.is_some() && !command_bool(command, "autocommit", true);
    if in_transaction {
        return Err(CommandError::new(72, "writeConcern is not allowed within a multi-statement transaction"));
    }
    Ok(())
}

/// Logical session carried by `lsid`. `key` stays `None` while the session has
/// no open transaction, so the command reads and writes committed data.
pub(crate) struct SessionContext {
    pub(crate) key: Option<String>,
}

impl SessionContext {
    pub(crate) fn new(command: &BsonDocument, store: &dyn MongoStorage) -> Result<Self, CommandError> {
        let Some(session) = transaction_key(command)
        else {
            return Ok(Self { key: None });
        };
        let Some(txn_number) = transaction_number(command)?
        else {
            return Ok(Self { key: None });
        };
        if command_bool(command, "startTransaction", false) {
            return match store.begin_transaction(&session, txn_number) {
                TransactionBegin::Started => Ok(Self { key: Some(session) }),
                TransactionBegin::AlreadyStarted => Err(conflicting_transaction_error()),
                TransactionBegin::TooOld { newer } => Err(transaction_too_old_error(command, txn_number, newer)),
            };
        }
        // Only operations inside a multi-document transaction carry
        // `autocommit: false`; every other `txnNumber` is a retryable write and
        // runs against committed data, as `mongod` does.
        if command_bool(command, "autocommit", true) {
            return Ok(Self { key: None });
        }
        match store.session_write_state(&session, txn_number) {
            SessionWriteState::Active => Ok(Self { key: Some(session) }),
            SessionWriteState::TooOld { newer } => Err(transaction_too_old_error(command, txn_number, newer)),
            SessionWriteState::AlreadyCommitted => Err(transaction_committed_error(txn_number)),
            SessionWriteState::AlreadyAborted => Err(no_such_transaction_error(txn_number)),
            SessionWriteState::NoTransaction { active } => Err(transaction_mismatch_error(txn_number, active)),
        }
    }

    pub(crate) fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}

/// Stable textual form of the `lsid` document, used as the session key.
pub(crate) fn transaction_key(command: &BsonDocument) -> Option<String> {
    match command.get("lsid") {
        Some(BsonValue::Document(document)) => Some(json::document_to_json(document)),
        _ => None,
    }
}

/// `txnNumber` must be a 64-bit integer, exactly as `mongod 8.0.9` requires.
pub(crate) fn transaction_number(command: &BsonDocument) -> Result<Option<i64>, CommandError> {
    match command.get("txnNumber") {
        Some(BsonValue::Int64(value)) => Ok(Some(*value)),
        Some(BsonValue::Int32(_)) => Err(CommandError::new(14, "BSON field 'request.txnNumber' is the wrong type 'int', expected type 'long'")),
        Some(_) => Err(CommandError::new(14, "BSON field 'request.txnNumber' is the wrong type, expected type 'long'")),
        None => Ok(None),
    }
}

/// Session text in the `TransactionTooOld` message: the UUID followed by the
/// empty user id hash `mongod 8.0.9` prints for a session without `uid`.
pub(crate) fn session_text(command: &BsonDocument) -> String {
    let uuid = match command.get("lsid") {
        Some(BsonValue::Document(lsid)) => match lsid.get("id") {
            Some(BsonValue::Binary(binary)) if binary.bytes.len() == 16 => format!(
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                binary.bytes[0],
                binary.bytes[1],
                binary.bytes[2],
                binary.bytes[3],
                binary.bytes[4],
                binary.bytes[5],
                binary.bytes[6],
                binary.bytes[7],
                binary.bytes[8],
                binary.bytes[9],
                binary.bytes[10],
                binary.bytes[11],
                binary.bytes[12],
                binary.bytes[13],
                binary.bytes[14],
                binary.bytes[15]
            ),
            _ => String::new(),
        },
        _ => String::new(),
    };
    format!("{uuid} - 47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU= -  -  ")
}

pub(crate) fn transaction_too_old_error(command: &BsonDocument, txn_number: i64, newer: i64) -> CommandError {
    CommandError::new(
        225,
        format!(
            "Cannot start transaction with {{ txnNumber: {txn_number} }} on session {}because a newer transaction with txnNumberAndRetryCounter {{ txnNumber: {newer}, txnRetryCounter: 0 }} has already started on this session.",
            session_text(command)
        ),
    )
}

pub(crate) fn conflicting_transaction_error() -> CommandError {
    CommandError::new(117, "Only servers in a sharded cluster can start a new transaction at the active transaction number")
}

pub(crate) fn transaction_committed_error(txn_number: i64) -> CommandError {
    CommandError::new(256, format!("Transaction with {{ txnNumber: {txn_number} }} has been committed."))
}

pub(crate) fn transaction_mismatch_error(txn_number: i64, active: i64) -> CommandError {
    CommandError::new(
        251,
        format!("Given transaction number {txn_number} does not match any in-progress transactions. The active transaction number is {active}"),
    )
    .with_detail("errorLabels", BsonValue::Array(vec![BsonValue::String("TransientTransactionError".to_string())]))
}

pub(crate) fn ok_reply() -> BsonDocument {
    let mut response = BsonDocument::new();
    response.push("ok".to_string(), BsonValue::Double(1.0));
    response
}

/// `mongod 8.0.9` reports a write conflict during plan execution with the
/// `TransientTransactionError` label and aborts the transaction.
pub(crate) fn write_conflict_error() -> CommandError {
    CommandError::new(
        112,
        "Caused by :: Write conflict during plan execution and yielding is disabled. :: Please retry your operation or multi-document transaction.",
    )
    .with_detail("errorLabels", BsonValue::Array(vec![BsonValue::String("TransientTransactionError".to_string())]))
}

pub(crate) fn no_such_transaction_error(txn_number: i64) -> CommandError {
    CommandError::new(251, format!("Transaction with {{ txnNumber: {txn_number} }} has been aborted."))
        .with_detail("errorLabels", BsonValue::Array(vec![BsonValue::String("TransientTransactionError".to_string())]))
}

/// 索引项键：索引键元组后接文档键。
///
/// 元组本身带长度前缀，所以「单字段元组」天然是「复合元组」的前缀，等值探针
/// 可以直接按前缀扫描，不需要再拼分隔符。

/// `commitTransaction` / `abortTransaction` 的参数校验。
///
/// 判定顺序与 `mongod 8.0.9` 的实测一致：先看 `autocommit` 与 `txnNumber` 的
/// 组合（`InvalidOptions` / `NotARetryableWriteCommand` / `CommandFailed`），最后
/// 才看命令是否跑在 `admin` 上。校验通过返回 `None`。
pub fn transaction_command_error(name: &str, command: &BsonDocument) -> Option<CommandError> {
    let autocommit = command.get("autocommit");
    let has_txn_number = command.get("txnNumber").is_some();
    let has_lsid = command.get("lsid").is_some();
    if matches!(autocommit, Some(BsonValue::Bool(true))) {
        return Some(CommandError::new(72, "Specifying autocommit=true is not allowed."));
    }
    if matches!(autocommit, Some(BsonValue::Bool(false))) && !has_txn_number {
        return Some(CommandError::new(72, "'autocommit' field requires a transaction number to also be specified"));
    }
    if has_txn_number && !has_lsid {
        return Some(CommandError::new(72, "Transaction number requires a session ID to also be specified"));
    }
    if has_txn_number && autocommit.is_none() {
        return Some(CommandError::new(
            50768,
            format!(
                "txnNumber may only be provided for multi-document transactions and retryable write commands. autocommit:false was not provided, and {name} is not a retryable write command."
            ),
        )
        .with_detail("codeName", BsonValue::String("NotARetryableWriteCommand".to_string())));
    }
    if !has_txn_number && autocommit.is_none() {
        return Some(
            CommandError::new(125, format!("{name} must be run within a transaction"))
                .with_detail("codeName", BsonValue::String("CommandFailed".to_string())),
        );
    }
    if command.database() != "admin" {
        return Some(CommandError::new(13, format!("{name} may only be run against the admin database.")));
    }
    None
}

/// `commitTransaction`: applies the session's staged writes, reporting a write
/// conflict when another transaction changed one of those documents first.
pub fn commit_transaction(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let Some(session) = transaction_key(command)
    else {
        return Err(CommandError::new(40414, "commitTransaction must be run within a session"));
    };
    let txn_number = transaction_number(command)?.unwrap_or(0);
    match store.transaction_commit(&session, txn_number) {
        TransactionCommit::Committed | TransactionCommit::AlreadyCommitted => Ok(ok_reply()),
        TransactionCommit::Conflict => Err(write_conflict_error()),
        TransactionCommit::AlreadyAborted => Err(no_such_transaction_error(txn_number)),
        TransactionCommit::TooOld { newer } => Err(transaction_too_old_error(command, txn_number, newer)),
        TransactionCommit::NoTransaction { active } => Err(transaction_mismatch_error(txn_number, active)),
    }
}

/// `abortTransaction`: discards the session's staged writes.
pub fn abort_transaction(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let Some(session) = transaction_key(command)
    else {
        return Err(CommandError::new(40414, "abortTransaction must be run within a session"));
    };
    let txn_number = transaction_number(command)?.unwrap_or(0);
    match store.transaction_abort(&session, txn_number) {
        TransactionAbort::Aborted => Ok(ok_reply()),
        TransactionAbort::AlreadyCommitted => Err(transaction_committed_error(txn_number)),
        TransactionAbort::AlreadyAborted => Err(no_such_transaction_error(txn_number)),
        TransactionAbort::TooOld { newer } => Err(transaction_too_old_error(command, txn_number, newer)),
        TransactionAbort::NoTransaction { active } => Err(transaction_mismatch_error(txn_number, active)),
    }
}
