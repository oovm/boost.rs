//! 内置角色表、权限展开与命令授权判定。
//!
//! `mongod 8.0.9` 只在用户某个角色在命令资源上授予了对应 action 时才执行命令。
//! 内置角色的权限展开是从真机 `usersInfo({showPrivileges: true})` 应答采集的，
//! 数据本体在 `authorization.roles.json`（数据库作用域角色按授权数据库展开，
//! 集群绑定的角色是固定资源）。自定义角色从 `<db>.system.roles` 读回后用同一套
//! 展开，因此“报告出来的权限”和“执行时判定的权限”永远只有一份。
//!
//! 权限合并规则与真机一致：同一资源的多条权限合成一条，action 按 `mongod` 的
//! 规范顺序合并，`anyAction` 顶掉整张表。

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::OnceLock,
};

use database_protocol_mongo::{
    bson::{BsonDocument, BsonValue, value_to_key},
    json,
};

use crate::mongo::{commands::CommandError, storage::MongoStorage};

/// 内置角色展开数据。
const ROLE_TABLE_JSON: &str = include_str!("authorization.roles.json");

/// 凭证集合：数据库级权限不会覆盖它们，只有精确命名空间的授权才会。
const CREDENTIAL_COLLECTIONS: &[&str] = &["system.users", "system.backup_users", "system.roles"];

/// `mongod` 的 action 枚举按字母序，只有三个排在末尾、一个被提前。
const TRAILING_ACTIONS: &[&str] = &["applyOps", "setChangeStreamState", "getChangeStreamState"];

/// 角色引用，形如 `{ role, db }`。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RoleRef {
    /// 角色名。
    pub role: String,
    /// 角色所在的数据库。
    pub db: String,
}

/// 一条权限：资源文档原样保留，动作按 `mongod` 顺序。
#[derive(Debug, Clone, PartialEq)]
pub struct Privilege {
    /// 资源文档（`{db, collection}` / `{cluster}` / `{anyResource}` / `{system_buckets}`）。
    pub resource: BsonDocument,
    /// 动作名。
    pub actions: Vec<String>,
}

/// 存储里的自定义角色文档。
#[derive(Debug, Clone, Default)]
pub struct StoredRole {
    /// 角色自带的权限。
    pub privileges: Vec<Privilege>,
    /// 继承的角色。
    pub roles: Vec<RoleRef>,
    /// 认证限制数组，原样保留。
    pub authentication_restrictions: Vec<BsonValue>,
}

/// 把一个角色名解析成存储文档；内置角色由表直接命中，不走这里。
pub trait RoleResolver {
    /// 返回存储文档，角色不存在时返回 `None`。
    fn resolve(&self, role: &str, db: &str) -> Option<StoredRole>;
}

/// 线条式解析器：既不读存储也不认自定义角色。
pub struct NoRoles;

impl RoleResolver for NoRoles {
    fn resolve(&self, _role: &str, _db: &str) -> Option<StoredRole> {
        None
    }
}

/// 从 `<db>.system.roles` 读取自定义角色，并按 `db.role` 缓存一次展开内的结果。
pub struct StoreRoleResolver<'a> {
    store: &'a dyn MongoStorage,
    cache: RefCell<HashMap<String, Option<StoredRole>>>,
}

impl<'a> StoreRoleResolver<'a> {
    /// 绑定一个存储后端。
    pub fn new(store: &'a dyn MongoStorage) -> Self {
        Self { store, cache: RefCell::new(HashMap::new()) }
    }
}

impl RoleResolver for StoreRoleResolver<'_> {
    fn resolve(&self, role: &str, db: &str) -> Option<StoredRole> {
        let key = format!("{db}.{role}");
        if let Some(cached) = self.cache.borrow().get(&key) {
            return cached.clone();
        }
        let resolved = read_stored_role(self.store, db, &key);
        self.cache.borrow_mut().insert(key, resolved.clone());
        resolved
    }
}

/// 读取一个 `<db>.system.roles` 文档。
fn read_stored_role(store: &dyn MongoStorage, db: &str, key: &str) -> Option<StoredRole> {
    let namespace = format!("{db}.system.roles");
    let bytes = store.get(&namespace, &value_to_key(&BsonValue::String(key.to_string()))).ok()??;
    let document = database_protocol_mongo::bson::parse_bson(&bytes).ok()?;
    let privileges = match document.get("privileges") {
        Some(BsonValue::Array(entries)) => entries.iter().filter_map(privilege_from_value).collect(),
        _ => Vec::new(),
    };
    let roles = match document.get("roles") {
        Some(BsonValue::Array(entries)) => entries.iter().filter_map(role_from_value).collect(),
        _ => Vec::new(),
    };
    let authentication_restrictions = match document.get("authenticationRestrictions") {
        Some(BsonValue::Array(entries)) => entries.clone(),
        _ => Vec::new(),
    };
    Some(StoredRole { privileges, roles, authentication_restrictions })
}

/// 把一条权限文档读成 [`Privilege`]；形态不对时跳过。
fn privilege_from_value(value: &BsonValue) -> Option<Privilege> {
    let BsonValue::Document(entry) = value else {
        return None;
    };
    let BsonValue::Document(resource) = entry.get("resource")? else {
        return None;
    };
    let actions = match entry.get("actions") {
        Some(BsonValue::Array(actions)) => {
            actions.iter().filter_map(|action| match action {
                BsonValue::String(text) => Some(text.clone()),
                _ => None,
            }).collect()
        }
        _ => Vec::new(),
    };
    Some(Privilege { resource: resource.clone(), actions })
}

/// 把一条角色引用文档读成 [`RoleRef`]；形态不对时跳过。
fn role_from_value(value: &BsonValue) -> Option<RoleRef> {
    let BsonValue::Document(entry) = value else {
        return None;
    };
    match (entry.get("role"), entry.get("db")) {
        (Some(BsonValue::String(role)), Some(BsonValue::String(db))) => Some(RoleRef { role: role.clone(), db: db.clone() }),
        _ => None,
    }
}

/// 内置角色数据。
struct RoleTables {
    db_scoped: HashMap<String, Vec<(String, Vec<String>)>>,
    cluster_bound: HashMap<String, Vec<Privilege>>,
}

impl RoleTables {
    /// 内置角色名，数据库作用域角色在前。
    fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.db_scoped.keys().map(String::as_str).collect();
        names.extend(self.cluster_bound.keys().map(String::as_str));
        names.sort_unstable();
        names
    }
}

/// 解析一次后复用。
fn tables() -> &'static RoleTables {
    static TABLES: OnceLock<RoleTables> = OnceLock::new();
    TABLES.get_or_init(|| parse_tables(ROLE_TABLE_JSON).expect("the embedded role table is valid extended JSON"))
}

/// 解析内嵌的角色表。
fn parse_tables(text: &str) -> Result<RoleTables, String> {
    let document = json::document_from_json(text)?;
    let mut db_scoped = HashMap::new();
    if let Some(BsonValue::Document(entries)) = document.get("dbScoped") {
        for (role, value) in entries.fields() {
            let BsonValue::Array(rows) = value else {
                return Err(format!("dbScoped.{role} is not an array"));
            };
            let mut rows_out = Vec::new();
            for row in rows {
                let BsonValue::Document(row) = row else {
                    return Err(format!("dbScoped.{role} holds a non-document row"));
                };
                let collection = match row.get("collection") {
                    Some(BsonValue::String(text)) => text.clone(),
                    _ => return Err(format!("dbScoped.{role} row has no collection")),
                };
                let actions = match row.get("actions") {
                    Some(BsonValue::Array(actions)) => actions
                        .iter()
                        .map(|action| match action {
                            BsonValue::String(text) => Ok(text.clone()),
                            _ => Err(format!("dbScoped.{role} holds a non-string action")),
                        })
                        .collect::<Result<Vec<_>, String>>()?,
                    _ => return Err(format!("dbScoped.{role} row has no actions")),
                };
                rows_out.push((collection, actions));
            }
            db_scoped.insert(role.clone(), rows_out);
        }
    }
    let mut cluster_bound = HashMap::new();
    if let Some(BsonValue::Document(entries)) = document.get("clusterBound") {
        for (role, value) in entries.fields() {
            let BsonValue::Array(rows) = value else {
                return Err(format!("clusterBound.{role} is not an array"));
            };
            let privileges = rows.iter().map(privilege_from_value).collect::<Option<Vec<_>>>();
            cluster_bound.insert(role.clone(), privileges.ok_or_else(|| format!("clusterBound.{role} holds an invalid privilege"))?);
        }
    }
    Ok(RoleTables { db_scoped, cluster_bound })
}

/// 全部内置角色名。
pub fn builtin_role_names() -> Vec<String> {
    tables().names().into_iter().map(str::to_string).collect()
}

/// `role` 是否是该数据库上的内置角色。
///
/// 集群绑定的角色（`clusterAdmin`、`root`、`*AnyDatabase`、`clusterMonitor`、
/// `clusterManager`、`hostManager`、`backup`、`restore`、`__system`）只在
/// `admin` 上解析；数据库作用域角色在任何数据库上都成立，包括不存在的。
pub fn builtin_role_exists(role: &str, db: &str) -> bool {
    let tables = tables();
    if tables.db_scoped.contains_key(role) {
        return true;
    }
    tables.cluster_bound.contains_key(role) && db == "admin"
}

/// `action` 是否是 `mongod` 接受的 action 名。
pub fn is_valid_action(action: &str) -> bool {
    if action == "anyAction" {
        return true;
    }
    let tables = tables();
    tables.db_scoped.values().flatten().any(|(_, actions)| actions.iter().any(|candidate| candidate == action))
        || tables.cluster_bound.values().flatten().any(|entry| entry.actions.iter().any(|candidate| candidate == action))
}

/// 角色展开结果。
#[derive(Debug, Clone, Default)]
pub struct Expansion {
    /// 合并后的权限集合。
    pub privileges: Vec<Privilege>,
    /// 展开过程中访问过的角色，按访问顺序。
    pub inherited_roles: Vec<RoleRef>,
    /// 自定义角色声明的认证限制（每条是一个数组）。
    pub authentication_restrictions: Vec<BsonValue>,
}

/// 按资源合并权限的累加器。
struct PrivilegeMerger {
    entries: Vec<MergedEntry>,
}

/// 合并中的一条权限。
struct MergedEntry {
    key: String,
    resource: BsonDocument,
    actions: Vec<String>,
    mixed: bool,
}

impl PrivilegeMerger {
    /// 空累加器。
    fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// 追加一条权限；同一资源重复出现时标记为混合，动作取并集。
    fn add(&mut self, resource: &BsonDocument, actions: &[String]) {
        let key = resource_key(resource);
        let index = match self.entries.iter().position(|entry| entry.key == key) {
            Some(index) => {
                self.entries[index].mixed = true;
                index
            }
            None => {
                self.entries.push(MergedEntry { key: key.clone(), resource: resource.clone(), actions: Vec::new(), mixed: false });
                self.entries.len() - 1
            }
        };
        let entry = &mut self.entries[index];
        if entry.actions.iter().any(|action| action == "anyAction") {
            return;
        }
        if actions.iter().any(|action| action == "anyAction") {
            entry.actions.clear();
            entry.actions.push("anyAction".to_string());
            return;
        }
        for action in actions {
            if !entry.actions.iter().any(|existing| existing == action) {
                entry.actions.push(action.clone());
            }
        }
    }

    /// 收尾：混合过的多动作资源按 `mongod` 的规范顺序排序。
    fn finish(self) -> Vec<Privilege> {
        self.entries
            .into_iter()
            .map(|mut entry| {
                if entry.mixed && entry.actions.len() > 1 {
                    entry.actions.sort_by(|left, right| compare_actions(left, right));
                }
                Privilege { resource: entry.resource, actions: entry.actions }
            })
            .collect()
    }
}

/// 资源身份，用于合并指向同一目标的权限。
fn resource_key(resource: &BsonDocument) -> String {
    if bool_field(resource, "cluster") {
        return "C".to_string();
    }
    if bool_field(resource, "anyResource") {
        return "A".to_string();
    }
    if let Some(BsonValue::String(buckets)) = resource.get("system_buckets") {
        return format!("B:{buckets}");
    }
    format!("D:{}|{}", string_field(resource, "db").unwrap_or_default(), string_field(resource, "collection").unwrap_or_default())
}

/// 文档里的布尔字段。
fn bool_field(document: &BsonDocument, key: &str) -> bool {
    matches!(document.get(key), Some(BsonValue::Bool(true)))
}

/// 文档里的字符串字段。
fn string_field<'a>(document: &'a BsonDocument, key: &str) -> Option<&'a str> {
    match document.get(key) {
        Some(BsonValue::String(text)) => Some(text),
        _ => None,
    }
}

/// `mongod` 的 action 顺序：三个 action 排在末尾，`updateSearchIndex` 被提前。
fn compare_actions(left: &str, right: &str) -> std::cmp::Ordering {
    let left_trailing = TRAILING_ACTIONS.iter().position(|action| *action == left);
    let right_trailing = TRAILING_ACTIONS.iter().position(|action| *action == right);
    if left_trailing.is_some() || right_trailing.is_some() {
        return match (left_trailing, right_trailing) {
            (None, _) => std::cmp::Ordering::Less,
            (_, None) => std::cmp::Ordering::Greater,
            (Some(left), Some(right)) => left.cmp(&right),
        };
    }
    let left_key = if left == "updateSearchIndex" { "listSearchIndexes~" } else { left };
    let right_key = if right == "updateSearchIndex" { "listSearchIndexes~" } else { right };
    left_key.cmp(right_key)
}

/// 合并重复资源并按 `mongod` 的规范顺序排列动作，等价于 `createRole` 存盘前的整理。
pub fn normalize_privileges(privileges: &[Privilege]) -> Vec<Privilege> {
    let mut merger = PrivilegeMerger::new();
    for entry in privileges {
        merger.add(&entry.resource, &entry.actions);
    }
    merger.finish()
}

/// 把角色集合展开成权限集合、继承角色表与认证限制。
///
/// 角色按 `(role, db)` 顺序访问；每个自定义角色先贡献自身权限，再访问继承的
/// 角色，与 `mongod` 一致。悬空的角色引用展开为空。
pub fn expand_roles(roles: &[RoleRef], resolver: &dyn RoleResolver) -> Expansion {
    let mut merger = PrivilegeMerger::new();
    let mut inherited_roles = Vec::new();
    let mut authentication_restrictions = Vec::new();
    let mut visited = Vec::new();

    let mut ordered = roles.to_vec();
    ordered.sort();

    for role in ordered {
        add_role(&role, resolver, &mut merger, &mut inherited_roles, &mut authentication_restrictions, &mut visited);
    }

    Expansion { privileges: merger.finish(), inherited_roles, authentication_restrictions }
}

/// 递归访问一个角色。
fn add_role(
    role: &RoleRef,
    resolver: &dyn RoleResolver,
    merger: &mut PrivilegeMerger,
    inherited_roles: &mut Vec<RoleRef>,
    authentication_restrictions: &mut Vec<BsonValue>,
    visited: &mut Vec<String>,
) {
    let key = format!("{}.{}", role.db, role.role);
    if visited.iter().any(|entry| entry == &key) {
        return;
    }
    visited.push(key);
    inherited_roles.push(role.clone());
    let tables = tables();
    if let Some(entries) = tables.db_scoped.get(&role.role) {
        for (collection, actions) in entries {
            let mut resource = BsonDocument::new();
            resource.push("db".to_string(), BsonValue::String(role.db.clone()));
            resource.push("collection".to_string(), BsonValue::String(collection.clone()));
            merger.add(&resource, actions);
        }
        return;
    }
    if let Some(entries) = tables.cluster_bound.get(&role.role) {
        for entry in entries {
            merger.add(&entry.resource, &entry.actions);
        }
        return;
    }
    let Some(stored) = resolver.resolve(&role.role, &role.db) else {
        return;
    };
    if !stored.authentication_restrictions.is_empty() {
        authentication_restrictions.push(BsonValue::Array(stored.authentication_restrictions.clone()));
    }
    for entry in &stored.privileges {
        merger.add(&entry.resource, &entry.actions);
    }
    for child in &stored.roles {
        add_role(child, resolver, merger, inherited_roles, authentication_restrictions, visited);
    }
}

/// 角色集合展开后的权限。
pub fn privileges_for_roles(roles: &[RoleRef], resolver: &dyn RoleResolver) -> Vec<Privilege> {
    expand_roles(roles, resolver).privileges
}

/// 一条权限的资源是否覆盖 `{db, collection}`。
fn covers(resource: &BsonDocument, db: &str, collection: &str) -> bool {
    if bool_field(resource, "anyResource") {
        return true;
    }
    if bool_field(resource, "cluster") || resource.get("system_buckets").is_some() {
        return false;
    }
    let resource_db = string_field(resource, "db").unwrap_or_default();
    let resource_collection = string_field(resource, "collection").unwrap_or_default();
    if !resource_db.is_empty() && resource_db != db {
        return false;
    }
    if resource_collection.is_empty() && CREDENTIAL_COLLECTIONS.contains(&collection) {
        return false;
    }
    if !resource_collection.is_empty() && resource_collection != collection {
        return false;
    }
    true
}

/// 一条权限的动作列表是否给出了 `action`；`anyAction` 给出全部。
fn has_action(actions: &[String], action: &str) -> bool {
    actions.iter().any(|entry| entry == action) || actions.iter().any(|entry| entry == "anyAction")
}

/// 是否有任一权限在 `{db, collection}` 上给出 `action`。
fn grants(privileges: &[Privilege], action: &str, db: &str, collection: &str) -> bool {
    privileges.iter().any(|entry| has_action(&entry.actions, action) && covers(&entry.resource, db, collection))
}

/// 是否有任一权限给出集群级 `action`（如 `listDatabases`）。
pub fn cluster_grants(privileges: &[Privilege], action: &str) -> bool {
    privileges.iter().any(|entry| {
        has_action(&entry.actions, action) && (bool_field(&entry.resource, "cluster") || bool_field(&entry.resource, "anyResource"))
    })
}

/// 是否有任一权限把 `database` 放进授权范围。
pub fn database_in_scope(privileges: &[Privilege], database: &str) -> bool {
    privileges.iter().any(|entry| !entry.actions.is_empty() && covers(&entry.resource, database, ""))
}

/// 一条命令需要满足的授权要求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    /// 需要的 action。
    pub action: String,
    /// 集群级要求。
    pub cluster: bool,
    /// 目标数据库（集群级要求忽略）。
    pub db: String,
    /// 目标集合（空串表示整个数据库）。
    pub collection: String,
}

impl Requirement {
    /// 数据库级要求。
    fn on(action: &str, db: &str, collection: &str) -> Self {
        Self { action: action.to_string(), cluster: false, db: db.to_string(), collection: collection.to_string() }
    }

    /// 集群级要求。
    fn cluster(action: &str) -> Self {
        Self { action: action.to_string(), cluster: true, db: String::new(), collection: String::new() }
    }
}

/// `not authorized` 应答，命令文档按 `mongod` 的口径回显（`pwd` 被抹掉）。
fn not_authorized(command: &BsonDocument) -> BsonDocument {
    let database = match command.get("$db") {
        Some(BsonValue::String(db)) => db.clone(),
        _ => "admin".to_string(),
    };
    CommandError::new(13, format!("not authorized on {database} to execute command {}", render_document(command))).into_reply()
}

/// CommandError → 错误应答文档。
trait IntoReply {
    /// 渲染成错误应答。
    fn into_reply(self) -> BsonDocument;
}

impl IntoReply for CommandError {
    fn into_reply(self) -> BsonDocument {
        crate::mongo::commands::error_document(&self)
    }
}

/// UUID 的文本形态。
fn uuid_text(bytes: &[u8]) -> String {
    let hex: Vec<String> = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let text = hex.concat();
    format!("{}-{}-{}-{}-{}", &text[0..8], &text[8..12], &text[12..16], &text[16..20], &text[20..])
}

/// 命令文档按 `mongod` 的方式回显。
fn render_document(command: &BsonDocument) -> String {
    if command.fields().is_empty() {
        return "{}".to_string();
    }
    let rendered: Vec<String> = command
        .fields()
        .iter()
        .map(|(key, value)| {
            if key == "pwd" {
                format!("{key}: \"xxx\"")
            }
            else {
                format!("{key}: {}", render_value(value))
            }
        })
        .collect();
    format!("{{ {} }}", rendered.join(", "))
}

/// Extended JSON 值按 `mongod` 的方式回显。
fn render_value(value: &BsonValue) -> String {
    match value {
        BsonValue::Null => "null".to_string(),
        BsonValue::String(text) => serde_json::to_string(text).unwrap_or_else(|_| format!("\"{text}\"")),
        BsonValue::Bool(flag) => flag.to_string(),
        BsonValue::Int32(number) => number.to_string(),
        BsonValue::Int64(number) => number.to_string(),
        BsonValue::Double(number) => format!("{number}"),
        BsonValue::Decimal128(number) => number.as_str().to_string(),
        BsonValue::Array(items) => {
            if items.is_empty() {
                "[]".to_string()
            }
            else {
                format!("[ {} ]", items.iter().map(render_value).collect::<Vec<_>>().join(", "))
            }
        }
        BsonValue::Document(document) => render_document_value(document),
        BsonValue::ObjectId(bytes) => format!("ObjectId('{}')", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>()),
        BsonValue::Binary(binary) => {
            let base64 = boost_crypto::encoding::base64_encode(&binary.bytes);
            if binary.subtype == 4 && binary.bytes.len() == 16 {
                format!("UUID(\"{}\")", uuid_text(&binary.bytes))
            }
            else {
                format!("BinData({}, \"{base64}\")", binary.subtype)
            }
        }
        BsonValue::DateTime(milliseconds) => format!("{{ $date: {{ $numberLong: \"{milliseconds}\" }} }}"),
        BsonValue::Timestamp { t, i } => format!("{{ $timestamp: {{ t: {t}, i: {i} }} }}"),
        other => render_scalar(other),
    }
}

/// 非文档值的标量回显。
fn render_scalar(value: &BsonValue) -> String {
    match value {
        BsonValue::MinKey | BsonValue::MaxKey => "1".to_string(),
        BsonValue::Regex { pattern, options } => format!("{{ pattern: \"{pattern}\", options: \"{options}\" }}"),
        BsonValue::Symbol(text) | BsonValue::Code(text) => serde_json::to_string(text).unwrap_or_default(),
        _ => "null".to_string(),
    }
}

/// 文档值的回显，含 `$binary` / 数值 / `$oid` 这几个 `mongod` 特别处理的分支。
fn render_document_value(document: &BsonDocument) -> String {
    if let Some(BsonValue::Document(binary)) = document.get("$binary") {
        let base64 = string_field(binary, "base64").unwrap_or_default();
        let subtype = string_field(binary, "subType").unwrap_or("00");
        let decoded = boost_crypto::encoding::base64_decode(base64).unwrap_or_default();
        if subtype == "04" && decoded.len() == 16 {
            return format!("UUID(\"{}\")", uuid_text(&decoded));
        }
        let numeric = u8::from_str_radix(subtype, 16).unwrap_or(0);
        return format!("BinData({numeric}, \"{base64}\")");
    }
    for tag in ["$numberInt", "$numberLong", "$numberDouble", "$numberDecimal"] {
        if let Some(value) = document.get(tag) {
            return render_value(value);
        }
    }
    if let Some(BsonValue::String(identifier)) = document.get("$oid") {
        return format!("ObjectId('{identifier}')");
    }
    if document.fields().is_empty() {
        return "{}".to_string();
    }
    let rendered: Vec<String> = document.fields().iter().map(|(key, value)| format!("{key}: {}", render_value(value))).collect();
    format!("{{ {} }}", rendered.join(", "))
}

/// 把 `db.collection` 命名空间拆开，裸名字默认落在命令数据库上。
fn namespace_target(namespace: &str, database: &str) -> Option<(String, String)> {
    if namespace.is_empty() {
        return None;
    }
    match namespace.split_once('.') {
        None => Some((database.to_string(), namespace.to_string())),
        Some((db, collection)) if !db.is_empty() && !collection.is_empty() => Some((db.to_string(), collection.to_string())),
        _ => None,
    }
}

/// 可能以普通数值或标签化数值出现的非负整数。
fn number_field(value: &BsonValue) -> Option<usize> {
    match value {
        BsonValue::Int32(number) if *number >= 0 => Some(*number as usize),
        BsonValue::Int64(number) if *number >= 0 => Some(*number as usize),
        BsonValue::Double(number) if *number >= 0.0 && number.fract() == 0.0 => Some(*number as usize),
        _ => None,
    }
}

/// `bulkWrite` 通过 `nsInfo` 能触达的每个命名空间。
fn bulk_write_requirements(command: &BsonDocument, database: &str) -> Vec<Requirement> {
    let info = match command.get("nsInfo") {
        Some(BsonValue::Array(entries)) => entries.clone(),
        _ => Vec::new(),
    };
    let ops = match command.get("ops") {
        Some(BsonValue::Array(entries)) => entries.clone(),
        _ => Vec::new(),
    };
    let mut requirements = Vec::new();
    for op in &ops {
        let BsonValue::Document(op) = op else {
            continue;
        };
        let insert = op.get("insert").and_then(number_field);
        let update = op.get("update").and_then(number_field);
        let remove = op.get("delete").and_then(number_field);
        let index = insert.or(update).or(remove);
        let Some(index) = index else {
            continue;
        };
        let target = info.get(index).and_then(|entry| match entry {
            BsonValue::Document(entry) => string_field(entry, "ns").and_then(|ns| namespace_target(ns, database)),
            _ => None,
        });
        let Some((db, collection)) = target else {
            continue;
        };
        let action = if insert.is_some() { "insert" } else if update.is_some() { "update" } else { "remove" };
        requirements.push(Requirement::on(action, &db, &collection));
    }
    requirements
}

/// `$out` 或 `$merge.into` 命名的目标集合，默认落在命令数据库上。
fn output_target(value: &BsonValue, database: &str) -> Option<(String, String)> {
    match value {
        BsonValue::String(collection) if !collection.is_empty() => Some((database.to_string(), collection.clone())),
        BsonValue::Document(document) => {
            let collection = string_field(document, "coll").filter(|collection| !collection.is_empty())?;
            Some((string_field(document, "db").unwrap_or(database).to_string(), collection.to_string()))
        }
        _ => None,
    }
}

/// 一条聚合阶段引用的 `from` / `into`。
fn stage_field(stage: &BsonDocument, operator: &str, field: &str) -> Option<BsonValue> {
    match stage.get(operator) {
        Some(BsonValue::Document(document)) => document.get(field).cloned(),
        _ => None,
    }
}

/// 本服务实现的命令各自需要的 action。
fn requirements_for(name: &str, command: &BsonDocument, database: &str) -> Option<Vec<Requirement>> {
    let field = |key: &str| string_field(command, key).unwrap_or_default().to_string();
    Some(match name {
        "find" => vec![Requirement::on("find", database, &field("find"))],
        "count" => vec![Requirement::on("find", database, &field("count"))],
        "distinct" => vec![Requirement::on("find", database, &field("distinct"))],
        "aggregate" => {
            let pipeline = match command.get("pipeline") {
                Some(BsonValue::Array(pipeline)) => pipeline.clone(),
                _ => Vec::new(),
            };
            if let Some(BsonValue::Document(first)) = pipeline.first() {
                if first.get("$currentOp").is_some() {
                    return Some(vec![Requirement::cluster("inprog")]);
                }
            }
            let mut requirements = vec![Requirement::on("find", database, &field("aggregate"))];
            for stage in &pipeline {
                let BsonValue::Document(stage) = stage else {
                    continue;
                };
                // `$lookup`、`$graphLookup` 与 `$unionWith` 读同库的另一个集合，
                // `mongod` 要求那上面的 `find`；对象形态的 `from` 指外部数据库，
                // 由管道校验器拒绝，因此不折算成授权。
                for value in [stage_field(stage, "$lookup", "from"), stage_field(stage, "$graphLookup", "from"), stage.get("$unionWith").cloned()] {
                    if let Some(BsonValue::String(namespace)) = value {
                        if !namespace.is_empty() {
                            requirements.push(Requirement::on("find", database, &namespace));
                        }
                    }
                }
                // `$out` / `$merge` 写到另一个集合，没有写动作的只读用户不能借
                // 管道改数据。
                if let Some((db, collection)) = stage.get("$out").and_then(|value| output_target(value, database)) {
                    requirements.push(Requirement::on("insert", &db, &collection));
                    requirements.push(Requirement::on("remove", &db, &collection));
                }
                if let Some((db, collection)) = stage_field(stage, "$merge", "into").and_then(|value| output_target(&value, database)) {
                    requirements.push(Requirement::on("insert", &db, &collection));
                    requirements.push(Requirement::on("update", &db, &collection));
                }
            }
            requirements
        }
        "explain" => {
            if let Some(BsonValue::Document(explained)) = command.get("explain") {
                if let Some(inner) = explained.command_name() {
                    if inner != "explain" {
                        if let Some(requirements) = requirements_for(inner, explained, database) {
                            return Some(requirements);
                        }
                    }
                }
            }
            vec![Requirement::on("find", database, "")]
        }
        "insert" => vec![Requirement::on("insert", database, &field("insert"))],
        "update" => vec![Requirement::on("update", database, &field("update"))],
        "delete" => vec![Requirement::on("remove", database, &field("delete"))],
        "findAndModify" => {
            let collection = field("findAndModify");
            // `remove: true` 删除命中文档，需要 `remove` 而不是 `update`。
            let action = if matches!(command.get("remove"), Some(BsonValue::Bool(true))) { "remove" } else { "update" };
            vec![Requirement::on("find", database, &collection), Requirement::on(action, database, &collection)]
        }
        "bulkWrite" => bulk_write_requirements(command, database),
        "create" => {
            let collection = field("create");
            let mut requirements = vec![Requirement::on("createCollection", database, &collection)];
            // 视图要读源集合才能应答查询，`mongod` 在创建之外还要求源的读动作。
            if let Some(view_on) = string_field(command, "viewOn").filter(|view| !view.is_empty()) {
                requirements.push(Requirement::on("find", database, view_on));
            }
            requirements
        }
        "drop" => vec![Requirement::on("dropCollection", database, &field("drop"))],
        "collMod" => vec![Requirement::on("collMod", database, &field("collMod"))],
        "createIndexes" => vec![Requirement::on("createIndex", database, &field("createIndexes"))],
        "dropIndexes" => vec![Requirement::on("dropIndex", database, &field("dropIndexes"))],
        "listIndexes" => vec![Requirement::on("listIndexes", database, &field("listIndexes"))],
        "listCollections" => vec![Requirement::on("listCollections", database, "")],
        "dbStats" => vec![Requirement::on("dbStats", database, "")],
        "collStats" => vec![Requirement::on("collStats", database, &field("collStats"))],
        "dropDatabase" => vec![Requirement::on("dropDatabase", database, "")],
        "createUser" => vec![Requirement::on("createUser", database, "")],
        "dropUser" => vec![Requirement::on("dropUser", database, "")],
        "usersInfo" => vec![Requirement::on("viewUser", database, "")],
        "getMore" => vec![Requirement::on("find", database, &field("collection"))],
        "killCursors" => vec![Requirement::on("killCursors", database, &field("killCursors"))],
        "dbHash" => vec![Requirement::on("dbHash", database, "")],
        "validate" => vec![Requirement::on("validate", database, &field("validate"))],
        "renameCollection" => rename_collection_targets(command, database),
        "createRole" => create_role_targets(command, database),
        "updateRole" => vec![Requirement::on("updateRole", database, "")],
        "dropRole" => vec![Requirement::on("dropRole", database, "")],
        "grantRolesToUser" => grant_roles_targets(command, database, "grantRole"),
        "revokeRolesFromUser" => grant_roles_targets(command, database, "revokeRole"),
        "grantRolesToRole" => grant_roles_targets(command, database, "grantRole"),
        "revokeRolesFromRole" => grant_roles_targets(command, database, "revokeRole"),
        "grantPrivilegesToRole" => {
            let mut requirements = vec![Requirement::on("grantRole", database, "")];
            requirements.extend(privilege_targets(command.get("privileges"), "grantRole", database));
            requirements
        }
        "updateUser" => update_user_targets(command, database),
        "invalidateUserCache" => vec![Requirement::cluster("invalidateUserCache")],
        "serverStatus" => vec![Requirement::cluster("serverStatus")],
        "hostInfo" => vec![Requirement::cluster("hostInfo")],
        "currentOp" => vec![Requirement::cluster("inprog")],
        "killOp" => vec![Requirement::cluster("killop")],
        "replSetGetStatus" => vec![Requirement::cluster("replSetGetStatus")],
        "replSetGetConfig" => vec![Requirement::cluster("replSetGetConfig")],
        "replSetInitiate" => vec![Requirement::cluster("replSetConfigure")],
        _ => return None,
    })
}

/// `createUser` 的要求：创建动作加上每一个被授予角色所在库的 `grantRole`。
///
/// 没有逐角色检查，`userAdmin@app` 可以造出 `root@admin` 账号再拿它用，那是
/// 提权而不是委派管理。
fn create_user_targets(command: &BsonDocument, database: &str) -> Vec<Requirement> {
    let mut requirements = vec![Requirement::on("createUser", database, "")];
    let roles = match command.get("roles") {
        Some(BsonValue::Array(roles)) => roles.clone(),
        _ => Vec::new(),
    };
    for role in &roles {
        // 形态不对的角色条目交给命令校验器，应答 `BadValue` 而不是授权失败。
        if let BsonValue::String(_) = role {
            requirements.push(Requirement::on("grantRole", database, ""));
            continue;
        }
        if let BsonValue::Document(entry) = role {
            if let (Some(BsonValue::String(_)), Some(BsonValue::String(db))) = (entry.get("role"), entry.get("db")) {
                requirements.push(Requirement::on("grantRole", db, ""));
            }
        }
    }
    requirements
}

/// `[{ role, db }]` 形态的角色列表，跳过形态不对的条目。
fn role_targets(roles: Option<&BsonValue>) -> Vec<RoleRef> {
    match roles {
        Some(BsonValue::Array(entries)) => entries.iter().filter_map(role_from_value).collect(),
        _ => Vec::new(),
    }
}

/// `privileges` 数组命名的资源各自需要的 action。
fn privilege_targets(privileges: Option<&BsonValue>, action: &str, database: &str) -> Vec<Requirement> {
    let Some(BsonValue::Array(entries)) = privileges else {
        return Vec::new();
    };
    let mut targets = Vec::new();
    for entry in entries {
        let BsonValue::Document(entry) = entry else {
            continue;
        };
        let Some(BsonValue::Document(resource)) = entry.get("resource") else {
            continue;
        };
        if let Some(db) = string_field(resource, "db") {
            targets.push(Requirement::on(action, db, string_field(resource, "collection").unwrap_or_default()));
        }
        else if bool_field(resource, "anyResource") {
            targets.push(Requirement::on(action, database, ""));
        }
    }
    targets
}

/// `renameCollection` 的要求：源需要 `renameCollectionSameDB`、目标需要 `dropCollection`。
fn rename_collection_targets(command: &BsonDocument, database: &str) -> Vec<Requirement> {
    let from = string_field(command, "renameCollection").and_then(|namespace| namespace_target(namespace, database));
    let to = string_field(command, "to").and_then(|namespace| namespace_target(namespace, database));
    match (from, to) {
        (Some((from_db, from_collection)), Some((to_db, to_collection))) => vec![
            Requirement::on("renameCollectionSameDB", &from_db, &from_collection),
            Requirement::on("dropCollection", &to_db, &to_collection),
        ],
        _ => vec![Requirement::on("renameCollectionSameDB", database, "")],
    }
}

/// `createRole`：创建动作加上每个被授予资源的 `grantRole`。
fn create_role_targets(command: &BsonDocument, database: &str) -> Vec<Requirement> {
    let mut requirements = vec![Requirement::on("createRole", database, "")];
    requirements.extend(role_targets(command.get("roles")).into_iter().map(|role| Requirement::on("grantRole", &role.db, "")));
    requirements.extend(privilege_targets(command.get("privileges"), "grantRole", database));
    requirements
}

/// `grantRolesToUser` / `revokeRolesFromUser` / `grantRolesToRole` / `revokeRolesFromRole`。
fn grant_roles_targets(command: &BsonDocument, database: &str, action: &str) -> Vec<Requirement> {
    let mut requirements = vec![Requirement::on(action, database, "")];
    requirements.extend(role_targets(command.get("roles")).into_iter().map(|role| Requirement::on(action, &role.db, "")));
    requirements
}

/// `updateUser`：每个被改的字段一个动作，每个被授予或收回的角色一个角色动作。
fn update_user_targets(command: &BsonDocument, database: &str) -> Vec<Requirement> {
    let mut requirements = Vec::new();
    if matches!(command.get("pwd"), Some(BsonValue::String(_))) {
        requirements.push(Requirement::on("changePassword", database, ""));
    }
    if command.get("customData").is_some() {
        requirements.push(Requirement::on("changeCustomData", database, ""));
    }
    if command.get("authenticationRestrictions").is_some() {
        requirements.push(Requirement::on("setAuthenticationRestriction", database, ""));
    }
    if command.get("roles").is_some() {
        requirements.push(Requirement::on("revokeRole", database, ""));
        requirements.extend(role_targets(command.get("roles")).into_iter().map(|role| Requirement::on("grantRole", &role.db, "")));
    }
    if requirements.is_empty() {
        requirements.push(Requirement::on("changeCustomData", database, ""));
    }
    requirements
}

/// `usersInfo` 需要调用者有权查看的数据库。
fn users_info_targets(command: &BsonDocument, database: &str) -> Vec<Requirement> {
    if let Some(BsonValue::Document(selection)) = command.get("usersInfo") {
        if matches!(selection.get("forAllDBs"), Some(BsonValue::Bool(true))) {
            return vec![Requirement::cluster("viewUser")];
        }
    }
    let mut selections: Vec<String> = Vec::new();
    collect_user_databases(command.get("usersInfo"), database, &mut selections);
    selections.into_iter().map(|db| Requirement::on("viewUser", &db, "")).collect()
}

/// 递归读取 `usersInfo` 选择器里的数据库。
fn collect_user_databases(value: Option<&BsonValue>, database: &str, selections: &mut Vec<String>) {
    match value {
        Some(BsonValue::String(_)) | None => selections.push(database.to_string()),
        Some(BsonValue::Array(items)) => {
            for item in items {
                collect_user_databases(Some(item), database, selections);
            }
        }
        Some(BsonValue::Document(document)) => match string_field(document, "db") {
            Some(db) => selections.push(db.to_string()),
            None => selections.push(database.to_string()),
        },
        Some(_) => selections.push(database.to_string()),
    }
}

/// 一条要求是否被权限集合满足。
fn check(privileges: &[Privilege], requirement: &Requirement) -> bool {
    if requirement.cluster {
        return cluster_grants(privileges, &requirement.action);
    }
    grants(privileges, &requirement.action, &requirement.db, &requirement.collection)
}

/// `true` 表示集合名属于凭证集合。
fn credential_collection(name: &str) -> bool {
    CREDENTIAL_COLLECTIONS.contains(&name)
}

/// 已通过授权、但 `mongod` 仍以专用错误拒绝的凭证集合操作。
///
/// 凭证集合不能被 `drop`，也不能作为 `$out` / `$merge` 的目标；缺少权限的
/// 调用方在前面的授权检查里已经得到 13，只有授权通过者才看到这些码。
fn credential_refusal(name: &str, command: &BsonDocument, database: &str) -> Option<BsonDocument> {
    if name == "drop" {
        if let Some(BsonValue::String(collection)) = command.get("drop") {
            if credential_collection(collection) {
                return Some(
                    CommandError::new(20, format!("cannot drop system collection {database}.{collection}"))
                        .with_detail("codeName", BsonValue::String("IllegalOperation".to_string()))
                        .into_reply(),
                );
            }
        }
    }
    if name != "aggregate" {
        return None;
    }
    let pipeline = match command.get("pipeline") {
        Some(BsonValue::Array(pipeline)) => pipeline.clone(),
        _ => Vec::new(),
    };
    for stage in &pipeline {
        let BsonValue::Document(stage) = stage else {
            continue;
        };
        if let Some((_, collection)) = stage.get("$out").and_then(|value| output_target(value, database)) {
            if credential_collection(&collection) {
                return Some(
                    CommandError::new(17385, format!("Can't $out to special collection: {collection}"))
                        .with_detail("codeName", BsonValue::String("Location17385".to_string()))
                        .into_reply(),
                );
            }
        }
        if let Some((_, collection)) = stage_field(stage, "$merge", "into").and_then(|value| output_target(&value, database)) {
            if credential_collection(&collection) {
                return Some(
                    CommandError::new(31319, format!("Cannot $merge to special collection: {collection}"))
                        .with_detail("codeName", BsonValue::String("Location31319".to_string()))
                        .into_reply(),
                );
            }
        }
    }
    None
}

/// 已认证会话的授权判定。
///
/// 允许时返回 `None`；否则返回可直接写回线的错误应答。`listDatabases` 的拒绝
/// 保留自己的文案，`mongod` 对它不回显命令文档。
pub fn authorize_command(roles: &[RoleRef], command: &BsonDocument, resolver: &dyn RoleResolver) -> Option<BsonDocument> {
    let name = command.command_name()?;
    if name == "listDatabases" {
        let privileges = privileges_for_roles(roles, resolver);
        if matches!(command.get("authorizedDatabases"), Some(BsonValue::Bool(false))) && !cluster_grants(&privileges, "listDatabases") {
            return Some(
                CommandError::new(13, "Insufficient permissions to list all databases").into_reply(),
            );
        }
        return None;
    }
    let database = match command.get("$db") {
        Some(BsonValue::String(db)) => db.clone(),
        _ => "admin".to_string(),
    };
    let privileges = privileges_for_roles(roles, resolver);
    // `root` 回答所有权限问题，包括它上报的权限表没有枚举到的 action。
    let root = roles.iter().any(|role| role.role == "root" && role.db == "admin");
    let requirements = match name {
        "usersInfo" => users_info_targets(command, &database),
        "createUser" => create_user_targets(command, &database),
        _ => match requirements_for(name, command, &database) {
            Some(requirements) => requirements,
            None => return None,
        },
    };
    for requirement in requirements {
        // `root` 回答所有被枚举的 action，但凭证集合始终按命名空间判定：真机
        // 拒绝 `root` 对 `authz.system.roles` 的 `find`，因为它持有的是
        // `admin.system.roles` 而不是任意数据库授权。
        let root_bypass = root && !credential_collection(&requirement.collection);
        if !root_bypass && !check(&privileges, &requirement) {
            return Some(not_authorized(command));
        }
    }
    credential_refusal(&name, command, &database)
}

/// 权限范围允许 `listDatabases` 看到的数据库名；`None` 表示无过滤。
pub fn database_scope(roles: &[RoleRef], databases: &[String], resolver: &dyn RoleResolver) -> Option<Vec<String>> {
    let privileges = privileges_for_roles(roles, resolver);
    if cluster_grants(&privileges, "listDatabases") {
        return None;
    }
    Some(databases.iter().filter(|database| database_in_scope(&privileges, database)).cloned().collect())
}

/// 权限集合转换成 BSON 数组（`usersInfo` 与 `listDatabases` 的应答形态）。
pub fn privileges_to_value(privileges: &[Privilege]) -> BsonValue {
    BsonValue::Array(
        privileges
            .iter()
            .map(|entry| {
                let mut document = BsonDocument::new();
                document.push("resource".to_string(), BsonValue::Document(entry.resource.clone()));
                document.push(
                    "actions".to_string(),
                    BsonValue::Array(entry.actions.iter().map(|action| BsonValue::String(action.clone())).collect()),
                );
                BsonValue::Document(document)
            })
            .collect(),
    )
}

/// 角色列表转换成 BSON 数组。
pub fn roles_to_value(roles: &[RoleRef]) -> BsonValue {
    BsonValue::Array(
        roles
            .iter()
            .map(|role| {
                let mut document = BsonDocument::new();
                document.push("role".to_string(), BsonValue::String(role.role.clone()));
                document.push("db".to_string(), BsonValue::String(role.db.clone()));
                BsonValue::Document(document)
            })
            .collect(),
    )
}

/// 从 BSON 数组读取角色列表，跳过形态不对的条目。
pub fn roles_from_value(value: Option<&BsonValue>) -> Vec<RoleRef> {
    match value {
        Some(BsonValue::Array(entries)) => entries.iter().filter_map(role_from_value).collect(),
        _ => Vec::new(),
    }
}

/// 从 BSON 数组读取权限列表，跳过形态不对的条目。
pub fn privileges_from_value(value: Option<&BsonValue>) -> Vec<Privilege> {
    match value {
        Some(BsonValue::Array(entries)) => entries.iter().filter_map(privilege_from_value).collect(),
        _ => Vec::new(),
    }
}
