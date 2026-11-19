//! MongoDB 认证合同：SCRAM-SHA-1 / SCRAM-SHA-256 会话、用户与角色管理命令。
//!
//! 行为对齐 `mongod 8.0.9`：错误码、错误文本、回复字段顺序与 `system.users` /
//! `system.roles` 的存储形状都是合同的一部分。密码学原语全部取自
//! `boost_crypto`（MD5 / SHA-1 / SHA-256 / HMAC / PBKDF2 / Base64），
//! SASLprep 取自 `unicode_stringprep`，熵源默认走 `boost_random` 的系统 CSPRNG，
//! 测试可注入 [`ScriptedEntropy`] 获得可复现 transcript。
//! 本模块不实现任何自有哈希、HMAC、PBKDF2 或字符串预处理。

use boost_crypto::{
    encoding::{base64_decode, base64_encode},
    hash::{md5, sha1, sha256},
    kdf::{pbkdf2_hmac_sha1, pbkdf2_hmac_sha256},
    mac::{hmac_sha1, hmac_sha256},
};
use database_protocol_mongo::bson::{BinaryData, BsonDocument, BsonValue, value_to_key};
use unicode_stringprep::SaslprepError;

use crate::mongo::{
    authorization::{
        self, Privilege, RoleRef, StoreRoleResolver, expand_roles, is_valid_action, normalize_privileges, privileges_to_value, roles_to_value,
    },
    commands,
    cursor::CursorStore,
    storage::MongoStorage,
};

/// SCRAM 机制。注册顺序（`saslStart` 候选）与报告顺序（`usersInfo`）不同，
/// 两者的差异是 `mongod` 合同的一部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScramMechanism {
    /// SCRAM-SHA-256（RFC 7677）。
    Sha256,
    /// SCRAM-SHA-1（RFC 5802）。
    Sha1,
}

impl ScramMechanism {
    /// 机制在 wire 与 `credentials` 子文档中的名字。
    pub fn name(self) -> &'static str {
        match self {
            Self::Sha256 => "SCRAM-SHA-256",
            Self::Sha1 => "SCRAM-SHA-1",
        }
    }

    /// 按精确大小写解析机制名；未知名字返回 `None`。
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "SCRAM-SHA-256" => Some(Self::Sha256),
            "SCRAM-SHA-1" => Some(Self::Sha1),
            _ => None,
        }
    }

    fn key_length(self) -> usize {
        match self {
            Self::Sha256 => 32,
            Self::Sha1 => 20,
        }
    }

    fn salt_length(self) -> usize {
        match self {
            Self::Sha256 => 24,
            Self::Sha1 => 16,
        }
    }

    fn iterations(self) -> u32 {
        match self {
            Self::Sha256 => 15_000,
            Self::Sha1 => 10_000,
        }
    }
}

/// `saslStart` 接受的机制顺序。
pub const SCRAM_MECHANISMS: [ScramMechanism; 2] = [ScramMechanism::Sha256, ScramMechanism::Sha1];

/// `usersInfo` 报告中 `mechanisms` 与 `credentials` 的枚举顺序。
pub const USER_REPORT_MECHANISMS: [ScramMechanism; 2] = [ScramMechanism::Sha1, ScramMechanism::Sha256];

/// 熵源合同：生产走系统 CSPRNG，测试注入确定性字节。
pub trait Entropy {
    /// 用随机字节填充 `out`，失败时返回原因。
    fn fill_bytes(&mut self, out: &mut [u8]) -> Result<(), String>;
}

/// 基于 `boost_random` 系统 CSPRNG 的默认熵源。
pub struct SystemEntropy {
    inner: boost_random::Secure<boost_random::SystemEntropy>,
}

impl SystemEntropy {
    /// 创建系统熵源。
    pub fn new() -> Self {
        Self { inner: boost_random::Secure::system() }
    }
}

impl Default for SystemEntropy {
    fn default() -> Self {
        Self::new()
    }
}

impl Entropy for SystemEntropy {
    fn fill_bytes(&mut self, out: &mut [u8]) -> Result<(), String> {
        self.inner.fill(out).map_err(|error| error.to_string())
    }
}

/// 回放固定字节序列的熵源，用于可复现的测试与夹具。
pub struct ScriptedEntropy {
    bytes: Vec<u8>,
    position: usize,
}

impl ScriptedEntropy {
    /// 以给定字节序列创建熵源，`fill_bytes` 从头部顺序消耗。
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self { bytes: bytes.into(), position: 0 }
    }
}

impl Entropy for ScriptedEntropy {
    fn fill_bytes(&mut self, out: &mut [u8]) -> Result<(), String> {
        let remaining = self.bytes.len() - self.position;
        if remaining < out.len() {
            return Err("scripted entropy exhausted".to_string());
        }
        out.copy_from_slice(&self.bytes[self.position..self.position + out.len()]);
        self.position += out.len();
        Ok(())
    }
}

/// 已认证的连接身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthIdentity {
    /// 用户名。
    pub user: String,
    /// 认证库。
    pub db: String,
}

/// 进行中的 SASL 会话。状态只由本模块的命令推进。
#[derive(Debug, Clone)]
pub struct ScramConversation {
    user: String,
    db: String,
    mechanism: ScramMechanism,
    client_first_bare: String,
    server_first: String,
    stored_key: Vec<u8>,
    server_key: Vec<u8>,
    roles: Vec<RoleRef>,
    phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Final,
    Finish,
}

/// 单连接的认证状态：已注册用户、授予角色与唯一 SASL 会话槽。
#[derive(Debug, Clone, Default)]
pub struct AuthState {
    /// 已认证用户（`saslContinue` 完成时登记）。
    pub users: Vec<AuthIdentity>,
    /// 已认证用户持有的角色。
    pub roles: Vec<RoleRef>,
    /// 当前 SASL 会话；`conversationId` 合同恒为 1。
    pub conversation: Option<ScramConversation>,
}

impl AuthState {
    /// 创建空认证状态。
    pub fn new() -> Self {
        Self::default()
    }
}

/// 认证命令的执行上下文：存储、游标表、认证状态与熵源。
pub struct AuthSession<'a> {
    /// 权威存储。
    pub store: &'a dyn MongoStorage,
    /// 调用方游标表（`find` 原语需要）。
    pub cursors: &'a mut CursorStore,
    /// 本连接的认证状态，由连接持有者拥有并跨命令复用。
    pub auth: &'a mut AuthState,
    /// 熵源。
    pub entropy: &'a mut dyn Entropy,
}

impl<'a> AuthSession<'a> {
    /// 以调用方持有的存储、游标表、认证状态与熵源创建执行上下文。
    pub fn new(store: &'a dyn MongoStorage, cursors: &'a mut CursorStore, auth: &'a mut AuthState, entropy: &'a mut dyn Entropy) -> Self {
        Self { store, cursors, auth, entropy }
    }
}

/// `verify_client_final` 的失败种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScramVerifyError {
    /// transcript 与存储凭证不符，`mongod` 回 `AuthenticationFailed`。
    AuthenticationFailed,
}

/// 纯函数 SCRAM client-final 校验：重放 transcript 并返回服务器签名。
///
/// 不触碰任何会话状态，供 RFC 5802 / RFC 7677 官方向量直接重放，
/// 也是 [`sasl_continue`] 的校验内核。
pub fn verify_client_final(
    mechanism: ScramMechanism,
    stored_key: &[u8],
    server_key: &[u8],
    client_first_bare: &str,
    server_first: &str,
    client_final: &str,
) -> Result<Vec<u8>, ScramVerifyError> {
    let nonce_end = server_first.find(',').unwrap_or(server_first.len());
    let nonce = server_first.get(2..nonce_end).unwrap_or("");
    let mut fields: Vec<(&str, &str)> = Vec::new();
    for part in client_final.split(',') {
        let Some(equals) = part.find('=')
        else {
            continue;
        };
        if equals == 0 {
            continue;
        }
        let key = &part[..equals];
        let value = &part[equals + 1..];
        match fields.iter_mut().find(|(existing, _)| *existing == key) {
            Some(entry) => entry.1 = value,
            None => fields.push((key, value)),
        }
    }
    let value_of = |key: &str| fields.iter().find(|(name, _)| *name == key).map(|(_, value)| *value);
    let Some(proof_text) = value_of("p")
    else {
        return Err(ScramVerifyError::AuthenticationFailed);
    };
    if value_of("r") != Some(nonce) {
        return Err(ScramVerifyError::AuthenticationFailed);
    }
    // JS 模板对缺失属性打印 "undefined"，保持与实测行为一致。
    let channel = value_of("c").unwrap_or("undefined");
    let r = value_of("r").unwrap_or("undefined");
    let without_proof = format!("c={channel},r={r}");
    let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
    let Some(client_signature) = mac(mechanism, stored_key, auth_message.as_bytes())
    else {
        return Err(ScramVerifyError::AuthenticationFailed);
    };
    let Ok(proof) = base64_decode(proof_text)
    else {
        return Err(ScramVerifyError::AuthenticationFailed);
    };
    if proof.len() != mechanism.key_length() {
        return Err(ScramVerifyError::AuthenticationFailed);
    }
    let client_key: Vec<u8> = proof.iter().zip(client_signature.iter()).map(|(left, right)| left ^ right).collect();
    if digest(mechanism, &client_key) != stored_key {
        return Err(ScramVerifyError::AuthenticationFailed);
    }
    mac(mechanism, server_key, auth_message.as_bytes()).ok_or(ScramVerifyError::AuthenticationFailed)
}

/// `createUser` 命令。
pub fn create_user(session: &mut AuthSession, command: &BsonDocument) -> BsonDocument {
    let db = database_of(command);
    let Some(BsonValue::String(name)) = command.get("createUser")
    else {
        return bad_value("createUser must be a string");
    };
    let Some(BsonValue::String(password)) = command.get("pwd")
    else {
        return auth_error(40_414, "IDLFailedToParse", "BSON field 'createUser.pwd' is missing but a required field");
    };
    let Some(BsonValue::Array(roles)) = command.get("roles")
    else {
        return auth_error(40_414, "IDLFailedToParse", "BSON field 'createUser.roles' is missing but a required field");
    };
    if find_user(session, &db, name).is_some() {
        return auth_error(51_003, "Location51003", format!("User \"{name}@{db}\" already exists"));
    }

    let mechanisms: Vec<ScramMechanism> = match command.get("mechanisms") {
        Some(BsonValue::Array(requested)) => {
            let mut parsed: Vec<ScramMechanism> = Vec::new();
            for entry in requested {
                let BsonValue::String(text) = entry
                else {
                    return auth_error(2, "BadValue", format!("Unknown auth mechanism '{}'", value_to_text(entry)));
                };
                let Some(mechanism) = ScramMechanism::parse(text)
                else {
                    return auth_error(2, "BadValue", format!("Unknown auth mechanism '{text}'"));
                };
                parsed.push(mechanism);
            }
            if parsed.is_empty() {
                return auth_error(2, "BadValue", "mechanisms must not be empty");
            }
            parsed
        }
        _ => SCRAM_MECHANISMS.to_vec(),
    };

    let mut granted: Vec<RoleRef> = Vec::new();
    for requested in roles {
        let role = match parse_role_name(requested, &db) {
            Ok(role) => role,
            Err(error) => return error,
        };
        if !role_exists(session.store, &role) {
            return auth_error(31, "RoleNotFound", format!("Could not find role: {}@{}", role.role, role.db));
        }
        granted.push(role);
    }

    let mut credentials = BsonDocument::new();
    for mechanism in mechanisms {
        match make_credentials(mechanism, name, password, session.entropy) {
            Ok(document) => credentials.push(mechanism.name(), BsonValue::Document(document)),
            Err(CredentialError::Saslprep(kind)) => {
                let label = match kind {
                    SaslprepError::Bidi => "CHECK_BIDI",
                    SaslprepError::Prohibited => "PROHIBITED",
                };
                return auth_error(50_692, "Location50692", format!("Error preflighting normalization: U_STRINGPREP_{label}_ERROR"));
            }
            Err(CredentialError::Other) => {
                return auth_error(18, "AuthenticationFailed", "Authentication failed.");
            }
        }
    }

    let mut user_document = BsonDocument::new();
    user_document.push("_id", BsonValue::String(format!("{db}.{name}")));
    let mut user_id = vec![0u8; 16];
    if session.entropy.fill_bytes(&mut user_id).is_err() {
        return auth_error(18, "AuthenticationFailed", "Authentication failed.");
    }
    user_document.push("userId", BsonValue::Binary(BinaryData::new(0x04, user_id)));
    user_document.push("user", BsonValue::String(name.clone()));
    user_document.push("db", BsonValue::String(db.clone()));
    user_document.push("credentials", BsonValue::Document(credentials));
    user_document.push("roles", roles_to_value(&granted));
    let reply = insert_one(session.store, "system.users", &db, user_document);
    if has_write_errors(&reply) {
        return auth_error(51_003, "Location51003", format!("User \"{name}@{db}\" already exists"));
    }
    ok_reply()
}

/// `dropUser` 命令。
pub fn drop_user(session: &mut AuthSession, command: &BsonDocument) -> BsonDocument {
    let db = database_of(command);
    let Some(BsonValue::String(name)) = command.get("dropUser")
    else {
        return bad_value("dropUser must be a string");
    };
    if find_user(session, &db, name).is_none() {
        return auth_error(11, "UserNotFound", format!("User '{name}@{db}' not found"));
    }
    let mut filter = BsonDocument::new();
    filter.push("user", BsonValue::String(name.clone()));
    delete_one(session.store, "system.users", &db, filter);
    ok_reply()
}

const CREATE_ROLE_FIELDS: [&str; 4] = ["createRole", "privileges", "roles", "authenticationRestrictions"];

const COMMON_COMMAND_FIELDS: [&str; 23] = [
    "$db",
    "lsid",
    "$lsid",
    "$readPreference",
    "$clusterTime",
    "$audit",
    "$client",
    "$configServer",
    "$shard",
    "$dbVersion",
    "$direct",
    "comment",
    "writeConcern",
    "readConcern",
    "maxTimeMS",
    "txnNumber",
    "autocommit",
    "startTransaction",
    "shellBatch",
    "aggregationBatchSize",
    "replWriteConcern",
    "provenance",
    "stmtId",
];

/// `createRole` 命令。
pub fn create_role(session: &mut AuthSession, command: &BsonDocument) -> BsonDocument {
    let db = database_of(command);
    let name = match command.get("createRole") {
        Some(BsonValue::String(text)) => text.clone(),
        other => return type_mismatch("createRole.createRole", type_name(other), "string"),
    };
    for (key, _) in &command.fields {
        if !CREATE_ROLE_FIELDS.contains(&key.as_str()) && !COMMON_COMMAND_FIELDS.contains(&key.as_str()) {
            return idl_unknown(&format!("createRole.{key}"));
        }
    }
    let Some(privileges_value) = command.get("privileges")
    else {
        return idl_missing("createRole.privileges");
    };
    let Some(roles_value) = command.get("roles")
    else {
        return idl_missing("createRole.roles");
    };
    let BsonValue::Array(privilege_entries) = privileges_value
    else {
        return type_mismatch("createRole.privileges", type_name(Some(privileges_value)), "array");
    };
    let BsonValue::Array(role_entries) = roles_value
    else {
        return type_mismatch("createRole.roles", type_name(Some(roles_value)), "array");
    };
    if let Some(restrictions) = command.get("authenticationRestrictions") {
        if !matches!(restrictions, BsonValue::Array(_)) {
            return type_mismatch("createRole.authenticationRestrictions", type_name(Some(restrictions)), "array");
        }
    }
    if name.is_empty() {
        return bad_value("Role name must be non-empty");
    }
    if authorization::builtin_role_names().iter().any(|builtin| builtin == &name) {
        return bad_value("Cannot create roles with the same name as a built-in role");
    }
    if find_role(session, &db, &name).is_some() {
        return auth_error(51_002, "Location51002", format!("Role \"{name}@{db}\" already exists"));
    }

    let mut granted: Vec<RoleRef> = Vec::new();
    for requested in role_entries {
        let role = match parse_role_name(requested, &db) {
            Ok(role) => role,
            Err(error) => return error,
        };
        if role.role == name && role.db == db {
            return auth_error(49, "InvalidRoleModification", format!("Cannot grant role {name}@{db} to itself."));
        }
        if db != "admin" && role.db != db {
            return auth_error(
                49,
                "InvalidRoleModification",
                format!("Roles on the '{db}' database cannot be granted roles from other databases"),
            );
        }
        if !role_exists(session.store, &role) {
            return auth_error(
                31,
                "RoleNotFound",
                format!("Cannot grant roles to '{name}@{db}': Could not find role: {}@{}", role.role, role.db),
            );
        }
        if !granted.iter().any(|entry| entry.db == role.db && entry.role == role.role) {
            granted.push(role);
        }
    }

    let mut privileges: Vec<Privilege> = Vec::new();
    for (index, entry) in privilege_entries.iter().enumerate() {
        let BsonValue::Document(entry_document) = entry
        else {
            return type_mismatch(&format!("createRole.privileges.{index}"), type_name(Some(entry)), "object");
        };
        if entry_document.get("resource").is_none() {
            return idl_missing("createRole.privileges.resource");
        }
        if entry_document.get("actions").is_none() {
            return idl_missing("createRole.privileges.actions");
        }
        let Some(BsonValue::Array(action_entries)) = entry_document.get("actions")
        else {
            return type_mismatch("createRole.privileges.actions", type_name(entry_document.get("actions")), "array");
        };
        if let Some(error) = validate_resource_shape(entry_document.get("resource").unwrap()) {
            return error;
        }
        if !resource_in_scope(entry_document.get("resource").unwrap(), &db) {
            return auth_error(
                49,
                "InvalidRoleModification",
                format!("Roles on the '{db}' database cannot be granted privileges that target other databases or the cluster"),
            );
        }
        let mut actions: Vec<String> = Vec::new();
        for (position, action_value) in action_entries.iter().enumerate() {
            let BsonValue::String(action) = action_value
            else {
                return type_mismatch(&format!("createRole.privileges.actions.{position}"), type_name(Some(action_value)), "string");
            };
            if !is_valid_action(action) {
                return bad_value(format!("Unknown action type in privilege set: '{action}'"));
            }
            actions.push(action.clone());
        }
        if actions.is_empty() {
            return bad_value("'actions' field of privilege resource must not be empty");
        }
        let resource = match entry_document.get("resource") {
            Some(BsonValue::Document(document)) => document.clone(),
            _ => BsonDocument::new(),
        };
        privileges.push(Privilege { resource, actions });
    }

    let mut role_document = BsonDocument::new();
    role_document.push("_id", BsonValue::String(format!("{db}.{name}")));
    role_document.push("role", BsonValue::String(name.clone()));
    role_document.push("db", BsonValue::String(db.clone()));
    role_document.push("privileges", privileges_to_value(&normalize_privileges(&privileges)));
    role_document.push("roles", roles_to_value(&granted));
    if let Some(BsonValue::Array(restrictions)) = command.get("authenticationRestrictions") {
        for entry in restrictions {
            let BsonValue::Document(entry_document) = entry
            else {
                return type_mismatch("createRole.authenticationRestrictions", type_name(Some(entry)), "object");
            };
            for (key, _) in &entry_document.fields {
                if key != "clientSource" && key != "serverAddress" {
                    return idl_unknown(&format!("createRole.authenticationRestrictions.{key}"));
                }
            }
            for key in ["clientSource", "serverAddress"] {
                if let Some(value) = entry_document.get(key) {
                    if !matches!(value, BsonValue::Array(_)) {
                        return type_mismatch(&format!("createRole.authenticationRestrictions.{key}"), type_name(Some(value)), "array");
                    }
                }
            }
        }
        role_document.push("authenticationRestrictions", BsonValue::Array(restrictions.clone()));
    }
    let reply = insert_one(session.store, "system.roles", &db, role_document);
    if has_write_errors(&reply) {
        return auth_error(51_002, "Location51002", format!("Role \"{name}@{db}\" already exists"));
    }
    ok_reply()
}

/// `dropRole` 命令：删除角色并从全部用户与角色中剥离引用。
pub fn drop_role(session: &mut AuthSession, command: &BsonDocument) -> BsonDocument {
    let db = database_of(command);
    let name = match command.get("dropRole") {
        Some(BsonValue::String(text)) => text.clone(),
        other => return type_mismatch("dropRole.dropRole", type_name(other), "string"),
    };
    for (key, _) in &command.fields {
        if key != "dropRole" && !COMMON_COMMAND_FIELDS.contains(&key.as_str()) {
            return idl_unknown(&format!("dropRole.{key}"));
        }
    }
    if authorization::builtin_role_names().iter().any(|builtin| builtin == &name) {
        return bad_value(format!("{name}@{db} is a built-in role and cannot be modified"));
    }
    if find_role(session, &db, &name).is_none() {
        return auth_error(31, "RoleNotFound", format!("Could not find role: {name}@{db}"));
    }
    let mut filter = BsonDocument::new();
    filter.push("_id", BsonValue::String(format!("{db}.{name}")));
    delete_one(session.store, "system.roles", &db, filter);
    for database in stored_databases(session.store) {
        for document in stored_users(session, &database) {
            strip_role_reference(session, "system.users", &database, &document, &name, &db);
        }
        for document in stored_roles(session, &database) {
            strip_role_reference(session, "system.roles", &database, &document, &name, &db);
        }
    }
    ok_reply()
}

/// `saslSupportedMechs`（`hello` speculation）的三种结果。
#[derive(Debug, Clone, PartialEq)]
pub enum SupportedMechs {
    /// 无 `saslSupportedMechs` 字段或用户不存在：回复不含该字段。
    None,
    /// 用户存在：机制名列表。
    Mechanisms(Vec<String>),
    /// 参数非法：作为 `hello` 错误返回的文档。
    Error(BsonDocument),
}

/// 解析 `hello.saslSupportedMechs`（`"db.user"` 形式）。
pub fn sasl_supported_mechs(session: &mut AuthSession, value: &BsonValue) -> SupportedMechs {
    let BsonValue::String(text) = value
    else {
        return SupportedMechs::None;
    };
    let message = "UserName must contain a '.' separated database.user pair";
    let Some(dot) = text.find('.')
    else {
        return SupportedMechs::Error(bad_value(message));
    };
    if dot == 0 || dot == text.len() - 1 {
        return SupportedMechs::Error(bad_value(message));
    }
    let db = &text[..dot];
    let user = &text[dot + 1..];
    match find_user(session, db, user) {
        None => SupportedMechs::None,
        Some(document) => SupportedMechs::Mechanisms(credential_list(&document)),
    }
}

/// `saslStart` 命令。
pub fn sasl_start(session: &mut AuthSession, command: &BsonDocument) -> BsonDocument {
    start_conversation(session, command)
}

/// `saslContinue` 命令。
pub fn sasl_continue(session: &mut AuthSession, command: &BsonDocument) -> BsonDocument {
    let identifier_ok = numeric_of(command.get("conversationId")) == Some(1.0);
    if session.auth.conversation.is_none() || !identifier_ok {
        return auth_error(17, "ProtocolError", "No SASL session state found");
    }
    if session.auth.conversation.as_ref().is_some_and(|state| state.phase == Phase::Finish) {
        let conversation = match session.auth.conversation.take() {
            Some(conversation) => conversation,
            None => return auth_error(17, "ProtocolError", "No SASL session state found"),
        };
        if !session.auth.users.iter().any(|entry| entry.user == conversation.user && entry.db == conversation.db) {
            session.auth.users.push(AuthIdentity { user: conversation.user.clone(), db: conversation.db.clone() });
            session.auth.roles.extend(conversation.roles.iter().cloned());
        }
        let mut reply = conversation_reply(true);
        reply.push("payload", binary_payload(""));
        reply.push("ok", BsonValue::Double(1.0));
        return reply;
    }
    let Some(payload) = payload_text(command.get("payload"))
    else {
        session.auth.conversation = None;
        return auth_error(10_270, "Location10270", "Failed decoding SASL payload: invalid base64");
    };
    let result = {
        let Some(conversation) = session.auth.conversation.as_ref()
        else {
            return auth_error(17, "ProtocolError", "No SASL session state found");
        };
        verify_client_final(
            conversation.mechanism,
            &conversation.stored_key,
            &conversation.server_key,
            &conversation.client_first_bare,
            &conversation.server_first,
            &payload,
        )
    };
    match result {
        Err(ScramVerifyError::AuthenticationFailed) => {
            session.auth.conversation = None;
            auth_error(18, "AuthenticationFailed", "Authentication failed.")
        }
        Ok(server_signature) => {
            if let Some(conversation) = session.auth.conversation.as_mut() {
                conversation.phase = Phase::Finish;
            }
            let mut reply = conversation_reply(false);
            reply.push("payload", binary_payload(&format!("v={}", base64_encode(&server_signature))));
            reply.push("ok", BsonValue::Double(1.0));
            reply
        }
    }
}

/// `logout` 命令：清空连接身份与会话。
pub fn logout(session: &mut AuthSession) -> BsonDocument {
    session.auth.users.clear();
    session.auth.roles.clear();
    session.auth.conversation = None;
    ok_reply()
}

/// `usersInfo` 命令。
pub fn users_info(session: &mut AuthSession, command: &BsonDocument) -> BsonDocument {
    let current_db = database_of(command);
    let parsed = parse_user_selection(command.get("usersInfo").unwrap_or(&BsonValue::Null), &current_db);
    if let Selection::Error(error) = &parsed {
        return error.clone();
    }
    let show_credentials = matches!(command.get("showCredentials"), Some(BsonValue::Bool(true)));
    let show_privileges = matches!(command.get("showPrivileges"), Some(BsonValue::Bool(true)));
    let mut documents: Vec<BsonDocument> = Vec::new();
    match parsed {
        Selection::AllDatabases => {
            for database in stored_databases(session.store) {
                documents.extend(stored_users(session, &database));
            }
        }
        Selection::AllInDb(database) => documents.extend(stored_users(session, &database)),
        Selection::Single(selections) => {
            let mut groups: Vec<(String, Vec<String>)> = Vec::new();
            for (database, user) in selections {
                match groups.iter_mut().find(|(db, _)| *db == database) {
                    Some((_, names)) => {
                        if !names.contains(&user) {
                            names.push(user);
                        }
                    }
                    None => groups.push((database, vec![user])),
                }
            }
            for (database, names) in groups {
                for document in stored_users(session, &database) {
                    if let Some(BsonValue::String(user)) = document.get("user") {
                        if names.contains(user) {
                            documents.push(document.clone());
                        }
                    }
                }
            }
        }
        Selection::Error(_) => unreachable!(),
    }
    let mut users: Vec<BsonValue> = Vec::new();
    for document in &documents {
        users.push(BsonValue::Document(user_report(session.store, document, show_credentials, show_privileges)));
    }
    let mut reply = BsonDocument::new();
    reply.push("users", BsonValue::Array(users));
    reply.push("ok", BsonValue::Double(1.0));
    reply
}

/// 未认证连接允许的命令集合。
pub fn allowed_before_auth(name: &str) -> bool {
    ["hello", "isMaster", "ismaster", "ping", "buildInfo", "connectionStatus", "saslStart", "saslContinue", "authenticate"].contains(&name)
}

/// 未认证连接执行受限命令时的固定错误。
pub fn authentication_required(name: &str) -> BsonDocument {
    auth_error(13, "Unauthorized", format!("Command {name} requires authentication"))
}

/// 任一库中是否存在至少一个用户（localhost exception 的判据）。
pub fn any_user_exists(store: &dyn MongoStorage) -> bool {
    for database in stored_databases(store) {
        let mut command = BsonDocument::new();
        command.push("count", BsonValue::String("system.users".to_string()));
        command.push("query", BsonValue::Document(BsonDocument::new()));
        command.push("$db", BsonValue::String(database));
        if let Ok(reply) = commands::count(&command, store) {
            if numeric_of(reply.get("n")).unwrap_or(f64::NAN) > 0.0 {
                return true;
            }
        }
    }
    false
}

#[derive(Debug)]
enum CredentialError {
    Saslprep(SaslprepError),
    Other,
}

fn start_conversation(session: &mut AuthSession, command: &BsonDocument) -> BsonDocument {
    let Some(BsonValue::String(mechanism_text)) = command.get("mechanism")
    else {
        return auth_error(40_414, "IDLFailedToParse", "BSON field 'saslStart.mechanism' is missing but a required field");
    };
    let Some(mechanism) = ScramMechanism::parse(mechanism_text)
    else {
        return auth_error(334, "MechanismUnavailable", format!("Received authentication for mechanism {mechanism_text} which is not enabled"));
    };
    let failed = || auth_error(18, "AuthenticationFailed", "Authentication failed.");
    let Some(payload) = payload_text(command.get("payload"))
    else {
        return if command.get("payload").is_none() {
            auth_error(40_414, "IDLFailedToParse", "BSON field 'saslStart.payload' is missing but a required field")
        }
        else {
            auth_error(10_270, "Location10270", "Failed decoding SASL payload: invalid base64")
        };
    };
    let comma = match payload.find(',') {
        Some(position) => position,
        None => return failed(),
    };
    let flag = &payload[..comma];
    if flag != "n" && flag != "y" {
        return failed();
    }
    let mut rest = &payload[comma + 1..];
    let mut authzid = "";
    if rest.starts_with("a=") {
        let Some(separator) = rest.find(',')
        else {
            return failed();
        };
        authzid = &rest[2..separator];
        rest = &rest[separator + 1..];
    }
    else if rest.starts_with(',') {
        rest = &rest[1..];
    }
    else {
        return failed();
    }
    let client_first_bare = rest;
    let bare = match client_first_bare.strip_prefix("n=") {
        Some(bare) => bare,
        None => return failed(),
    };
    let Some(second_comma) = bare.find(',')
    else {
        return failed();
    };
    let user = &bare[..second_comma];
    let client_nonce = match bare[second_comma + 1..].strip_prefix("r=") {
        Some(nonce) if !nonce.is_empty() => nonce,
        _ => return failed(),
    };
    if user.is_empty() || (!authzid.is_empty() && authzid != user) {
        return failed();
    }
    let db = string_of(command.get("db")).or_else(|| string_of(command.get("$db"))).unwrap_or_else(|| "admin".to_string());
    let Some(document) = find_user(session, &db, user)
    else {
        return failed();
    };
    let credentials = match document.get("credentials") {
        Some(BsonValue::Document(entry)) => Some(entry),
        _ => None,
    };
    let credential = match credentials.and_then(|entry| entry.get(mechanism.name())) {
        None | Some(BsonValue::Null) => {
            return auth_error(
                334,
                "MechanismUnavailable",
                format!("Unable to use {mechanism_text} based authentication for user without any {mechanism_text} credentials registered"),
            );
        }
        Some(value) => value,
    };
    let (salt, iterations, stored_key, server_key) = match credential {
        BsonValue::Document(entry) => (
            string_of(entry.get("salt")).unwrap_or_default(),
            numeric_of(entry.get("iterationCount")),
            base64_of(entry.get("storedKey")),
            base64_of(entry.get("serverKey")),
        ),
        _ => (String::new(), None, Vec::new(), Vec::new()),
    };
    let mut extension = vec![0u8; 24];
    if session.entropy.fill_bytes(&mut extension).is_err() {
        return failed();
    }
    let server_first =
        format!("r={}{},s={},i={}", client_nonce, base64_encode(&extension), salt, iterations.map_or_else(|| "NaN".to_string(), format_number));
    session.auth.conversation = Some(ScramConversation {
        user: user.to_string(),
        db,
        mechanism,
        client_first_bare: client_first_bare.to_string(),
        server_first: server_first.clone(),
        stored_key,
        server_key,
        roles: roles_of(&document),
        phase: Phase::Final,
    });
    let mut reply = conversation_reply(false);
    reply.push("payload", binary_payload(&server_first));
    reply.push("ok", BsonValue::Double(1.0));
    reply
}

fn make_credentials(mechanism: ScramMechanism, user: &str, password: &str, entropy: &mut dyn Entropy) -> Result<BsonDocument, CredentialError> {
    let processed = processed_password(mechanism, user, password)?;
    let mut salt = vec![0u8; mechanism.salt_length()];
    entropy.fill_bytes(&mut salt).map_err(|_| CredentialError::Other)?;
    let rounds = mechanism.iterations();
    let mut salted = vec![0u8; mechanism.key_length()];
    match mechanism {
        ScramMechanism::Sha256 => pbkdf2_hmac_sha256(processed.as_bytes(), &salt, rounds, &mut salted),
        ScramMechanism::Sha1 => pbkdf2_hmac_sha1(processed.as_bytes(), &salt, rounds, &mut salted),
    }
    .map_err(|_| CredentialError::Other)?;
    let client_key = mac(mechanism, &salted, b"Client Key").ok_or(CredentialError::Other)?;
    let stored_key = digest(mechanism, &client_key);
    let server_key = mac(mechanism, &salted, b"Server Key").ok_or(CredentialError::Other)?;
    let mut document = BsonDocument::new();
    document.push("iterationCount", BsonValue::Int32(rounds as i32));
    document.push("salt", BsonValue::String(base64_encode(&salt)));
    document.push("storedKey", BsonValue::String(base64_encode(&stored_key)));
    document.push("serverKey", BsonValue::String(base64_encode(&server_key)));
    Ok(document)
}

fn processed_password(mechanism: ScramMechanism, user: &str, password: &str) -> Result<String, CredentialError> {
    let prepared = unicode_stringprep::saslprep(password).map_err(CredentialError::Saslprep)?;
    if mechanism == ScramMechanism::Sha256 {
        return Ok(prepared);
    }
    Ok(hex_bytes(&md5(format!("{user}:mongo:{prepared}").as_bytes())))
}

fn digest(mechanism: ScramMechanism, data: &[u8]) -> Vec<u8> {
    match mechanism {
        ScramMechanism::Sha256 => sha256(data).to_vec(),
        ScramMechanism::Sha1 => sha1(data).to_vec(),
    }
}

fn mac(mechanism: ScramMechanism, key: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    match mechanism {
        ScramMechanism::Sha256 => hmac_sha256(key, data).ok().map(|tag| tag.to_vec()),
        ScramMechanism::Sha1 => hmac_sha1(key, data).ok().map(|tag| tag.to_vec()),
    }
}

fn parse_role_name(requested: &BsonValue, db: &str) -> Result<RoleRef, BsonDocument> {
    match requested {
        BsonValue::String(text) => Ok(RoleRef { role: text.clone(), db: db.to_string() }),
        BsonValue::Document(document) => {
            let Some(role_value) = document.get("role")
            else {
                return Err(bad_value("RoleName must contain a field named: role"));
            };
            let BsonValue::String(role_name) = role_value
            else {
                return Err(bad_value(format!(
                    "RoleName must contain a string field named: role. But, has type {}",
                    type_name(Some(role_value))
                )));
            };
            let Some(db_value) = document.get("db")
            else {
                return Err(bad_value("RoleName must contain a field named: db"));
            };
            let BsonValue::String(db_name) = db_value
            else {
                return Err(bad_value(format!("RoleName must contain a string field named: db. But, has type {}", type_name(Some(db_value)))));
            };
            Ok(RoleRef { role: role_name.clone(), db: db_name.clone() })
        }
        _ => Err(bad_value("Role name must be either a document or string")),
    }
}

fn validate_resource_shape(resource: &BsonValue) -> Option<BsonDocument> {
    let BsonValue::Document(document) = resource
    else {
        return None;
    };
    if document.get("systemBuckets").is_some() {
        return Some(bad_value("resource pattern must contain 'collection' or 'systemBuckets' specifier"));
    }
    let cluster = matches!(document.get("cluster"), Some(BsonValue::Bool(true)));
    let any_resource = matches!(document.get("anyResource"), Some(BsonValue::Bool(true)));
    if cluster && document.get("db").is_some() {
        return Some(bad_value("resource: {cluster: true} conflicts with resource type 'db'"));
    }
    if cluster && document.get("anyResource").is_some() {
        return Some(bad_value("resource: {cluster: true} conflicts with resource type 'anyResource'"));
    }
    if let Some(db_value) = document.get("db") {
        if !matches!(db_value, BsonValue::String(_)) {
            return Some(type_mismatch("createRole.privileges.resource.db", type_name(Some(db_value)), "string"));
        }
    }
    if let Some(collection) = document.get("collection") {
        if !matches!(collection, BsonValue::String(_)) {
            return Some(type_mismatch("createRole.privileges.resource.collection", type_name(Some(collection)), "string"));
        }
    }
    if cluster || any_resource {
        return None;
    }
    if matches!(document.get("db"), Some(BsonValue::String(_))) {
        if !matches!(document.get("collection"), Some(BsonValue::String(_))) {
            return Some(bad_value("resource pattern must contain 'collection' or 'systemBuckets' specifier"));
        }
        return None;
    }
    if document.get("collection").is_some() {
        return Some(bad_value("resource {collection: '...'} must include 'db' field as well"));
    }
    Some(bad_value("resource pattern must contain 'collection' or 'systemBuckets' specifier"))
}

fn resource_in_scope(resource: &BsonValue, role_db: &str) -> bool {
    if role_db == "admin" {
        return true;
    }
    matches!(
        resource,
        BsonValue::Document(document)
            if matches!(document.get("db"), Some(BsonValue::String(db)) if db == role_db)
    )
}

fn strip_role_reference(session: &mut AuthSession, collection: &str, database: &str, document: &BsonDocument, name: &str, role_db: &str) {
    let Some(BsonValue::Array(entries)) = document.get("roles")
    else {
        return;
    };
    let kept: Vec<BsonValue> = entries
        .iter()
        .filter(|entry| {
            !matches!(
                entry,
                BsonValue::Document(inner)
                    if matches!(inner.get("role"), Some(BsonValue::String(role)) if role == name)
                        && matches!(inner.get("db"), Some(BsonValue::String(db)) if db == role_db)
            )
        })
        .cloned()
        .collect();
    if kept.len() == entries.len() {
        return;
    }
    let Some(id_value) = document.get("_id")
    else {
        return;
    };
    let mut query = BsonDocument::new();
    query.push("_id", id_value.clone());
    let mut set = BsonDocument::new();
    set.push("roles", BsonValue::Array(kept));
    let mut operator = BsonDocument::new();
    operator.push("$set", BsonValue::Document(set));
    let mut spec = BsonDocument::new();
    spec.push("q", BsonValue::Document(query));
    spec.push("u", BsonValue::Document(operator));
    spec.push("limit", BsonValue::Int32(1));
    let mut command = BsonDocument::new();
    command.push("update", BsonValue::String(collection.to_string()));
    command.push("updates", BsonValue::Array(vec![BsonValue::Document(spec)]));
    command.push("$db", BsonValue::String(database.to_string()));
    let _ = commands::update(&command, session.store);
}

enum Selection {
    Single(Vec<(String, String)>),
    AllDatabases,
    AllInDb(String),
    Error(BsonDocument),
}

fn parse_user_selection(value: &BsonValue, current_db: &str) -> Selection {
    if let BsonValue::String(text) = value {
        return Selection::Single(vec![(current_db.to_string(), text.clone())]);
    }
    if let BsonValue::Array(entries) = value {
        if entries.is_empty() {
            return Selection::Error(bad_value("$or argument must be a non-empty array"));
        }
        let mut selections: Vec<(String, String)> = Vec::new();
        for entry in entries {
            match parse_user_selection(entry, current_db) {
                Selection::Single(items) => selections.extend(items),
                other => return other,
            }
        }
        return Selection::Single(selections);
    }
    if let Some(number) = numeric_of(Some(value)) {
        if number.is_finite() {
            if number.trunc() == 1.0 {
                return Selection::AllInDb(current_db.to_string());
            }
            return Selection::Error(bad_value("UserName must be either a string or an object"));
        }
    }
    let BsonValue::Document(document) = value
    else {
        return Selection::Error(bad_value("UserName must be either a string or an object"));
    };
    if matches!(document.get("forAllDBs"), Some(BsonValue::Bool(true))) {
        return Selection::AllDatabases;
    }
    for (key, _) in &document.fields {
        if key != "user" && key != "db" {
            return Selection::Error(bad_value(format!("UserName contains an unknown field named: '{key}")));
        }
    }
    let Some(user_value) = document.get("user")
    else {
        return Selection::Error(bad_value("UserName must contain a field named: user"));
    };
    let BsonValue::String(user) = user_value
    else {
        return Selection::Error(bad_value(format!(
            "UserName must contain a string field named: user. But, has type {}",
            type_name(Some(user_value))
        )));
    };
    let Some(db_value) = document.get("db")
    else {
        return Selection::Error(bad_value("UserName must contain a field named: db"));
    };
    let BsonValue::String(db) = db_value
    else {
        return Selection::Error(bad_value(format!(
            "UserName must contain a string field named: db. But, has type {}",
            type_name(Some(db_value))
        )));
    };
    Selection::Single(vec![(db.clone(), user.clone())])
}

fn user_report(store: &dyn MongoStorage, document: &BsonDocument, show_credentials: bool, show_privileges: bool) -> BsonDocument {
    let mut report = BsonDocument::new();
    for key in ["_id", "userId", "user", "db"] {
        if let Some(value) = document.get(key) {
            report.push(key, value.clone());
        }
    }
    let credentials = match document.get("credentials") {
        Some(BsonValue::Document(entry)) => Some(entry),
        _ => None,
    };
    let mechanisms: Vec<ScramMechanism> = USER_REPORT_MECHANISMS
        .iter()
        .copied()
        .filter(|mechanism| credentials.is_some_and(|entry| entry.get(mechanism.name()).is_some()))
        .collect();
    let mechanisms_value = BsonValue::Array(mechanisms.iter().map(|mechanism| BsonValue::String(mechanism.name().to_string())).collect());
    if show_privileges {
        report.push("mechanisms", mechanisms_value.clone());
    }
    if show_credentials {
        let mut credentials_report = BsonDocument::new();
        if let Some(stored) = credentials {
            for mechanism in &mechanisms {
                if let Some(value) = stored.get(mechanism.name()) {
                    credentials_report.push(mechanism.name(), value.clone());
                }
            }
        }
        report.push("credentials", BsonValue::Document(credentials_report));
    }
    let roles = sorted_roles(document);
    report.push("roles", roles_to_value(&roles));
    if !show_privileges {
        report.push("mechanisms", mechanisms_value);
    }
    if show_privileges {
        let expansion = expand_roles(&roles, &StoreRoleResolver::new(store));
        report.push("inheritedRoles", roles_to_value(&expansion.inherited_roles));
        report.push("inheritedPrivileges", privileges_to_value(&expansion.privileges));
        report.push("inheritedAuthenticationRestrictions", BsonValue::Array(expansion.authentication_restrictions));
    }
    report
}

fn roles_of(document: &BsonDocument) -> Vec<RoleRef> {
    let Some(BsonValue::Array(entries)) = document.get("roles")
    else {
        return Vec::new();
    };
    let user_db = match document.get("db") {
        Some(BsonValue::String(text)) => Some(text.clone()),
        _ => None,
    };
    let mut roles: Vec<RoleRef> = Vec::new();
    for entry in entries {
        match entry {
            BsonValue::String(text) => {
                if let Some(db) = &user_db {
                    roles.push(RoleRef { role: text.clone(), db: db.clone() });
                }
            }
            BsonValue::Document(inner) => {
                if let (Some(BsonValue::String(role)), Some(BsonValue::String(db))) = (inner.get("role"), inner.get("db")) {
                    roles.push(RoleRef { role: role.clone(), db: db.clone() });
                }
            }
            _ => {}
        }
    }
    roles
}

fn sorted_roles(document: &BsonDocument) -> Vec<RoleRef> {
    let mut roles = roles_of(document);
    roles.sort_by(|left, right| left.role.cmp(&right.role).then_with(|| left.db.cmp(&right.db)));
    roles
}

fn credential_list(document: &BsonDocument) -> Vec<String> {
    let credentials = match document.get("credentials") {
        Some(BsonValue::Document(entry)) => entry,
        _ => return Vec::new(),
    };
    SCRAM_MECHANISMS
        .iter()
        .filter(|mechanism| credentials.get(mechanism.name()).is_some())
        .map(|mechanism| mechanism.name().to_string())
        .collect()
}

fn find_user(session: &mut AuthSession, db: &str, user: &str) -> Option<BsonDocument> {
    let mut filter = BsonDocument::new();
    filter.push("user", BsonValue::String(user.to_string()));
    find_one(session, "system.users", db, filter)
}

fn find_role(session: &mut AuthSession, db: &str, name: &str) -> Option<BsonDocument> {
    let mut filter = BsonDocument::new();
    filter.push("_id", BsonValue::String(format!("{db}.{name}")));
    find_one(session, "system.roles", db, filter)
}

fn stored_users(session: &mut AuthSession, db: &str) -> Vec<BsonDocument> {
    find_all(session, "system.users", db)
}

fn stored_roles(session: &mut AuthSession, db: &str) -> Vec<BsonDocument> {
    find_all(session, "system.roles", db)
}

fn find_one(session: &mut AuthSession, collection: &str, db: &str, filter: BsonDocument) -> Option<BsonDocument> {
    let mut command = find_command(collection, db, filter);
    command.push("limit", BsonValue::Int32(1));
    let reply = commands::find(&command, session.store, session.cursors).ok()?;
    batch_documents(reply).into_iter().next()
}

fn find_all(session: &mut AuthSession, collection: &str, db: &str) -> Vec<BsonDocument> {
    let command = find_command(collection, db, BsonDocument::new());
    match commands::find(&command, session.store, session.cursors) {
        Ok(reply) => batch_documents(reply),
        Err(_) => Vec::new(),
    }
}

fn find_command(collection: &str, db: &str, filter: BsonDocument) -> BsonDocument {
    let mut command = BsonDocument::new();
    command.push("find", BsonValue::String(collection.to_string()));
    command.push("filter", BsonValue::Document(filter));
    command.push("$db", BsonValue::String(db.to_string()));
    command
}

fn batch_documents(reply: BsonDocument) -> Vec<BsonDocument> {
    let Some(BsonValue::Document(cursor)) = reply.get("cursor")
    else {
        return Vec::new();
    };
    let Some(BsonValue::Array(entries)) = cursor.get("firstBatch")
    else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| match entry {
            BsonValue::Document(document) => Some(document.clone()),
            _ => None,
        })
        .collect()
}

fn insert_one(store: &dyn MongoStorage, collection: &str, db: &str, document: BsonDocument) -> BsonDocument {
    let mut command = BsonDocument::new();
    command.push("insert", BsonValue::String(collection.to_string()));
    command.push("documents", BsonValue::Array(vec![BsonValue::Document(document)]));
    command.push("$db", BsonValue::String(db.to_string()));
    commands::insert(&command, store).unwrap_or_else(|error| commands::error_document(&error))
}

fn delete_one(store: &dyn MongoStorage, collection: &str, db: &str, filter: BsonDocument) {
    let mut spec = BsonDocument::new();
    spec.push("q", BsonValue::Document(filter));
    spec.push("limit", BsonValue::Int32(1));
    let mut command = BsonDocument::new();
    command.push("delete", BsonValue::String(collection.to_string()));
    command.push("deletes", BsonValue::Array(vec![BsonValue::Document(spec)]));
    command.push("$db", BsonValue::String(db.to_string()));
    let _ = commands::delete(&command, store);
}

fn stored_databases(store: &dyn MongoStorage) -> Vec<String> {
    let Ok(reply) = commands::list_databases(store)
    else {
        return Vec::new();
    };
    let Some(BsonValue::Array(entries)) = reply.get("databases")
    else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| match entry {
            BsonValue::Document(document) => match document.get("name") {
                Some(BsonValue::String(name)) => Some(name.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn role_exists(store: &dyn MongoStorage, role: &RoleRef) -> bool {
    authorization::builtin_role_exists(&role.role, &role.db) || stored_role_exists(store, &role.db, &role.role)
}

fn stored_role_exists(store: &dyn MongoStorage, db: &str, name: &str) -> bool {
    let key = value_to_key(&BsonValue::String(format!("{db}.{name}")));
    store.get(&format!("{db}.system.roles"), &key).ok().flatten().is_some()
}

fn has_write_errors(reply: &BsonDocument) -> bool {
    matches!(reply.get("writeErrors"), Some(BsonValue::Array(entries)) if !entries.is_empty())
}

fn payload_text(value: Option<&BsonValue>) -> Option<String> {
    match value? {
        BsonValue::Binary(data) => Some(String::from_utf8_lossy(&data.bytes).into_owned()),
        BsonValue::String(text) => base64_decode(text).ok().map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
        _ => None,
    }
}

fn base64_of(value: Option<&BsonValue>) -> Vec<u8> {
    match value {
        Some(BsonValue::String(text)) => base64_decode(text).unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn string_of(value: Option<&BsonValue>) -> Option<String> {
    match value {
        Some(BsonValue::String(text)) => Some(text.clone()),
        _ => None,
    }
}

fn database_of(command: &BsonDocument) -> String {
    string_of(command.get("$db")).unwrap_or_else(|| "admin".to_string())
}

fn numeric_of(value: Option<&BsonValue>) -> Option<f64> {
    match value {
        Some(BsonValue::Int32(number)) => Some(f64::from(*number)),
        Some(BsonValue::Int64(number)) => Some(*number as f64),
        Some(BsonValue::Double(number)) => Some(*number),
        _ => None,
    }
}

fn format_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value.trunc() == value && value.abs() < 9.007_199_254_740_992e12 {
        return format!("{}", value as i64);
    }
    format!("{value}")
}

fn type_name(value: Option<&BsonValue>) -> &'static str {
    match value {
        None => "undefined",
        Some(BsonValue::Null) => "null",
        Some(BsonValue::Array(_)) => "array",
        Some(BsonValue::String(_)) => "string",
        Some(BsonValue::Bool(_)) => "bool",
        Some(BsonValue::Int32(_)) => "int",
        Some(BsonValue::Int64(_)) => "long",
        Some(BsonValue::Double(_)) => "double",
        Some(BsonValue::Decimal128(_)) => "decimal",
        Some(BsonValue::Binary(_)) => "binData",
        _ => "object",
    }
}

fn value_to_text(value: &BsonValue) -> String {
    match value {
        BsonValue::String(text) => text.clone(),
        BsonValue::Int32(number) => number.to_string(),
        BsonValue::Int64(number) => number.to_string(),
        BsonValue::Double(number) => format_number(*number),
        BsonValue::Bool(true) => "true".to_string(),
        BsonValue::Bool(false) => "false".to_string(),
        BsonValue::Null => "null".to_string(),
        _ => "[object Object]".to_string(),
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn binary_payload(text: &str) -> BsonValue {
    BsonValue::Binary(BinaryData::new(0x00, text.as_bytes().to_vec()))
}

fn ok_reply() -> BsonDocument {
    let mut reply = BsonDocument::new();
    reply.push("ok", BsonValue::Double(1.0));
    reply
}

fn conversation_reply(done: bool) -> BsonDocument {
    let mut reply = BsonDocument::new();
    reply.push("conversationId", BsonValue::Int32(1));
    reply.push("done", BsonValue::Bool(done));
    reply
}

fn auth_error(code: i32, code_name: &str, message: impl Into<String>) -> BsonDocument {
    let mut reply = BsonDocument::new();
    reply.push("ok", BsonValue::Double(0.0));
    reply.push("errmsg", BsonValue::String(message.into()));
    reply.push("code", BsonValue::Int32(code));
    reply.push("codeName", BsonValue::String(code_name.to_string()));
    reply
}

fn bad_value(message: impl Into<String>) -> BsonDocument {
    auth_error(2, "BadValue", message)
}

fn type_mismatch(field: &str, actual: &str, expected: &str) -> BsonDocument {
    auth_error(14, "TypeMismatch", format!("BSON field '{field}' is the wrong type '{actual}', expected type '{expected}'"))
}

fn idl_missing(field: &str) -> BsonDocument {
    auth_error(40_414, "IDLFailedToParse", format!("BSON field '{field}' is missing but a required field"))
}

fn idl_unknown(field: &str) -> BsonDocument {
    auth_error(40_415, "IDLUnknownField", format!("BSON field '{field}' is an unknown field."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(text: &str) -> Vec<u8> {
        base64_decode(text).expect("valid base64")
    }

    #[test]
    fn rfc5802_scram_sha1_vector() {
        // RFC 5802 §5 官方 transcript（user/pass = pencil）。
        let client_first_bare = "n=user,r=fyko+d2lbbFgONRv9qkxdawL";
        let server_first = "r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096";
        let client_final = "c=biws,r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,p=v0X8v3Bz2T0CJGbJQyF0X+HI4Ts=";
        let expected_v = b64("rmF9pqV8S7suAoZWja4dJRkFsKQ=");
        let salt = b64("QSXCR+Q6sek8bf92");
        // RFC 5802 直接使用原文密码；`user:mongo:` MD5 包装是 MongoDB 特有规则。
        let processed = "pencil";
        let mut salted = [0u8; 20];
        pbkdf2_hmac_sha1(processed.as_bytes(), &salt, 4096, &mut salted).unwrap();
        let client_key = hmac_sha1(&salted, b"Client Key").unwrap();
        let stored_key = sha1(&client_key);
        let server_key = hmac_sha1(&salted, b"Server Key").unwrap();
        let signature = verify_client_final(ScramMechanism::Sha1, &stored_key, &server_key, client_first_bare, server_first, client_final)
            .expect("RFC 5802 transcript must verify");
        assert_eq!(signature, expected_v);
    }

    #[test]
    fn rfc7677_scram_sha256_vector() {
        // RFC 7677 §3 官方 transcript（user/pass = pencil）。
        let client_first_bare = "n=user,r=rOprNGfwEbeRWgbNEkqO";
        let server_first = "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
        let client_final = "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
        let expected_v = b64("6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=");
        let salt = b64("W22ZaJ0SNY7soEsUEjb6gQ==");
        let processed = unicode_stringprep::saslprep("pencil").unwrap();
        let mut salted = [0u8; 32];
        pbkdf2_hmac_sha256(processed.as_bytes(), &salt, 4096, &mut salted).unwrap();
        let client_key = hmac_sha256(&salted, b"Client Key").unwrap();
        let stored_key = sha256(&client_key);
        let server_key = hmac_sha256(&salted, b"Server Key").unwrap();
        let signature = verify_client_final(ScramMechanism::Sha256, &stored_key, &server_key, client_first_bare, server_first, client_final)
            .expect("RFC 7677 transcript must verify");
        assert_eq!(signature, expected_v);
    }

    #[test]
    fn verify_rejects_tampered_proof() {
        let salt = b64("QSXCR+Q6sek8bf92");
        let processed = "pencil";
        let mut salted = [0u8; 20];
        pbkdf2_hmac_sha1(processed.as_bytes(), &salt, 4096, &mut salted).unwrap();
        let client_key = hmac_sha1(&salted, b"Client Key").unwrap();
        let stored_key = sha1(&client_key);
        let server_key = hmac_sha1(&salted, b"Server Key").unwrap();
        let tampered = "c=biws,r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,p=AAAAAAAAAAAAAAAAAAAAAAAAAA==";
        assert_eq!(
            verify_client_final(
                ScramMechanism::Sha1,
                &stored_key,
                &server_key,
                "n=user,r=fyko+d2lbbFgONRv9qkxdawL",
                "r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096",
                tampered,
            ),
            Err(ScramVerifyError::AuthenticationFailed)
        );
    }

    #[test]
    fn mongodb_scram_sha1_credentials_match_node_crypto() {
        // 期望值由 node:crypto 独立生成（见 auth-vectors.mjs）。
        let mut entropy = ScriptedEntropy::new((0u8..16).collect::<Vec<u8>>());
        let credentials = make_credentials(ScramMechanism::Sha1, "alice", "s3cr3t", &mut entropy).expect("credentials");
        assert_eq!(credentials.get("iterationCount"), Some(&BsonValue::Int32(10_000)));
        assert_eq!(credentials.get("salt"), Some(&BsonValue::String("AAECAwQFBgcICQoLDA0ODw==".to_string())));
        assert_eq!(credentials.get("storedKey"), Some(&BsonValue::String("TT+VUHx/DKKjyYZvK10ouyUkQFs=".to_string())));
        assert_eq!(credentials.get("serverKey"), Some(&BsonValue::String("DYpf4eJGN6j2BLSquRUtn3nH2YU=".to_string())));
    }

    #[test]
    fn mongodb_scram_sha256_credentials_match_node_crypto() {
        let mut entropy = ScriptedEntropy::new((0u8..24).collect::<Vec<u8>>());
        let credentials = make_credentials(ScramMechanism::Sha256, "alice", "s3cr3t", &mut entropy).unwrap();
        assert_eq!(credentials.get("iterationCount"), Some(&BsonValue::Int32(15_000)));
        assert_eq!(credentials.get("storedKey"), Some(&BsonValue::String("r6YFkmZHrJJe/kVy0eFsKLYa6/5of3Bzo/sRub1TIFg=".to_string())));
        assert_eq!(credentials.get("serverKey"), Some(&BsonValue::String("JIGWX8RxsxBP9fke5iqwaYsFBoxEzMv1zFO3nrFK87o=".to_string())));
    }

    #[test]
    fn gs2_and_nonce_parsing_helpers() {
        assert_eq!(format_number(f64::NAN), "NaN");
        assert_eq!(format_number(15_000.0), "15000");
        assert_eq!(type_name(None), "undefined");
        assert_eq!(type_name(Some(&BsonValue::Int32(1))), "int");
        assert_eq!(type_name(Some(&BsonValue::Array(Vec::new()))), "array");
    }
}
