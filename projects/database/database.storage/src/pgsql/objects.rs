//! Session-independent SQL objects: databases, roles and table grants.
//!
//! `CREATE DATABASE`, `CREATE ROLE`, `GRANT` and `REVOKE` change a cluster-wide
//! catalog rather than a table, so they live in their own document next to the
//! schema and survive a restart. The document is strict: an unknown contract tag
//! or a malformed record is an error the caller reports instead of resetting the
//! catalog silently.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};

/// Contract tag written into every persisted object document.
pub const OBJECTS_CONTRACT: &str = "yyds-pg-objects/v0";

/// File name of the object catalog inside a data directory.
pub const OBJECTS_FILE: &str = "cluster-objects.json";

/// Table privilege a `GRANT` / `REVOKE` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PgPrivilege {
    /// `SELECT`.
    Select,
    /// `INSERT`.
    Insert,
    /// `UPDATE`.
    Update,
    /// `DELETE`.
    Delete,
}

impl PgPrivilege {
    /// Parses the privilege keyword, `None` for anything outside the subset.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_uppercase().as_str() {
            "SELECT" => Some(Self::Select),
            "INSERT" => Some(Self::Insert),
            "UPDATE" => Some(Self::Update),
            "DELETE" => Some(Self::Delete),
            _ => None,
        }
    }

    /// Canonical spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        }
    }
}

/// One role the cluster catalog knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PgRole {
    /// Role name.
    pub name: String,
    /// `SUPERUSER` flag; a superuser bypasses table grants.
    pub superuser: bool,
    /// Whether the role may create databases.
    pub createdb: bool,
    /// Whether the role may log in.
    pub login: bool,
    /// `INHERIT` flag: whether the role automatically uses the privileges of
    /// roles it is a member of. PostgreSQL defaults to `INHERIT`.
    #[serde(default = "default_true")]
    pub inherit: bool,
    /// Stored password verifier, present when the role was created with a
    /// `PASSWORD`. A role without one cannot authenticate against this catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<PgPassword>,
}

fn default_true() -> bool {
    true
}

/// One role-membership edge: `member` is a member of `role`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PgMembership {
    /// The role whose privileges are inherited.
    pub role: String,
    /// The role that inherits them.
    pub member: String,
}

/// Password verifier a role persists instead of its plaintext.
///
/// Both forms are derived once, when the password is set: the MD5 hash keeps
/// cleartext and MD5 logins working exactly like the listener's configured
/// secret, and the SCRAM verifier keeps `scram-sha-256` logins working without
/// the gateway ever reading the password back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PgPassword {
    /// `md5(password || role_name)` as lowercase hex.
    pub md5: String,
    /// SCRAM-SHA-256 verifier.
    pub scram: PgScramVerifier,
}

/// SCRAM-SHA-256 verifier parameters, all base64 except the iteration count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PgScramVerifier {
    /// Salt handed to the client in `server-first`.
    pub salt: String,
    /// PBKDF2 iteration count.
    pub iterations: u32,
    /// Base64 `StoredKey`.
    pub stored_key: String,
    /// Base64 `ServerKey`.
    pub server_key: String,
}

impl PgPassword {
    /// Derives both verifiers from one plaintext password.
    ///
    /// The SCRAM salt comes from the host random source, so two roles that pick
    /// the same password still carry different verifiers.
    pub fn derive(password: &str, role: &str) -> Result<Self, ObjectError> {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        use database_protocol_pgsql::auth::{ScramVerifier, md5_hex, verifier_for};

        let mut salt = [0u8; 16];
        boost_random::Secure::system()
            .fill(&mut salt)
            .map_err(|error| ObjectError::Persistence(format!("cannot salt a role password: {error}")))?;
        // PostgreSQL's default iteration count for `password_encryption = scram-sha-256`.
        const ITERATIONS: u32 = 4096;
        let ScramVerifier { stored_key, server_key } = verifier_for(password.as_bytes(), &salt, ITERATIONS);
        Ok(Self {
            md5: md5_hex(format!("{password}{role}").as_bytes()),
            scram: PgScramVerifier {
                salt: STANDARD.encode(salt),
                iterations: ITERATIONS,
                stored_key: STANDARD.encode(stored_key),
                server_key: STANDARD.encode(server_key),
            },
        })
    }

    /// Whether a cleartext login submitted `password` for `role`.
    pub fn verifies_cleartext(&self, password: &str, role: &str) -> bool {
        database_protocol_pgsql::auth::md5_hex(format!("{password}{role}").as_bytes()) == self.md5
    }

    /// `md5(md5(password || role) || salt)` response an MD5 login must submit.
    pub fn md5_response(&self, salt: &[u8]) -> String {
        let mut material = self.md5.clone().into_bytes();
        material.extend_from_slice(salt);
        format!("md5{}", database_protocol_pgsql::auth::md5_hex(&material))
    }

    /// Decoded SCRAM parameters, or `None` when the stored document is corrupt.
    pub fn scram(&self) -> Option<(database_protocol_pgsql::auth::ScramVerifier, Vec<u8>)> {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let salt = STANDARD.decode(&self.scram.salt).ok()?;
        let stored_key: [u8; 32] = STANDARD.decode(&self.scram.stored_key).ok()?.try_into().ok()?;
        let server_key: [u8; 32] = STANDARD.decode(&self.scram.server_key).ok()?.try_into().ok()?;
        Some((database_protocol_pgsql::auth::ScramVerifier { stored_key, server_key }, salt))
    }
}

/// One SQL sequence and its durable state.
///
/// `last_value` and `is_called` are PostgreSQL's pair: the next `nextval`
/// returns `last_value + increment` once `is_called` is set, and returns
/// `last_value` itself on the very first call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PgSequence {
    /// Sequence name.
    pub name: String,
    /// `START WITH` value, kept for `ALTER SEQUENCE ... RESTART` offsets.
    pub start: i64,
    /// `INCREMENT BY` step, negative for a descending sequence.
    pub increment: i64,
    /// Inclusive lower bound.
    pub min_value: i64,
    /// Inclusive upper bound.
    pub max_value: i64,
    /// Whether the sequence wraps instead of failing at its bound.
    pub cycle: bool,
    /// Last value handed out, or the value `RESTART` reset to.
    pub last_value: i64,
    /// Whether `last_value` was ever handed out.
    pub is_called: bool,
}

impl PgSequence {
    /// A sequence with the given `START`, `INCREMENT` and bounds.
    pub fn new(name: impl Into<String>, start: i64, increment: i64, min_value: i64, max_value: i64, cycle: bool) -> Self {
        Self { name: name.into(), start, increment, min_value, max_value, cycle, last_value: start, is_called: false }
    }

    /// PostgreSQL's default sequence over 64-bit values.
    pub fn ascending(name: impl Into<String>) -> Self {
        Self::new(name, 1, 1, 1, i64::MAX, false)
    }

    /// One `nextval` step, or `SequenceExhausted` at the bound.
    fn advance(&mut self) -> Result<i64, ObjectError> {
        let next = if self.is_called {
            self.last_value.checked_add(self.increment).ok_or(ObjectError::SequenceExhausted(self.name.clone()))?
        } else {
            self.last_value
        };
        let out_of_range = if self.increment > 0 { next > self.max_value } else { next < self.min_value };
        if out_of_range {
            if !self.cycle {
                return Err(ObjectError::SequenceExhausted(self.name.clone()));
            }
            // `CYCLE` wraps to the opposite bound.
            self.last_value = if self.increment > 0 { self.min_value } else { self.max_value };
        } else {
            self.last_value = next;
        }
        self.is_called = true;
        Ok(self.last_value)
    }
}

/// Fields one `ALTER SEQUENCE` changes. `None` keeps the current value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PgSequenceAlteration {
    /// `INCREMENT BY`.
    pub increment: Option<i64>,
    /// `RESTART [WITH n]`; `Some(None)` restarts at `start`.
    pub restart: Option<Option<i64>>,
    /// `MINVALUE n` or `NO MINVALUE`.
    pub min_value: Option<Option<i64>>,
    /// `MAXVALUE n` or `NO MAXVALUE`.
    pub max_value: Option<Option<i64>>,
    /// `CYCLE` / `NO CYCLE`.
    pub cycle: Option<bool>,
}

/// One database the cluster catalog knows and the options it was created with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PgDatabase {
    /// Database name.
    pub name: String,
    /// `OWNER` role; `postgres` unless the statement named one.
    pub owner: String,
    /// `ENCODING`. The gateway stores UTF-8 and rejects every other spelling.
    pub encoding: String,
    /// `TEMPLATE` the database was cloned from.
    pub template: String,
    /// `CONNECTION LIMIT`; `-1` means unlimited, exactly like PostgreSQL.
    pub connection_limit: i32,
}

impl PgDatabase {
    /// A database with PostgreSQL's defaults for an omitted option list.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            owner: "postgres".to_string(),
            encoding: "UTF8".to_string(),
            template: "template1".to_string(),
            connection_limit: -1,
        }
    }
}

/// One table privilege granted to a role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PgGrant {
    /// Grantee role name.
    pub role: String,
    /// Table name the privilege applies to.
    pub table: String,
    /// Granted privilege.
    pub privilege: PgPrivilege,
}

/// Failure raised by a catalog change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectError {
    /// The database already exists (`42P04`).
    DuplicateDatabase(String),
    /// The role already exists (`42710`).
    DuplicateRole(String),
    /// The role named by a grant or revoke does not exist (`42704`).
    UnknownRole(String),
    /// The membership would close a cycle (`0LP01`).
    CircularMembership {
        /// The role whose privileges were being granted.
        role: String,
        /// The member that already reaches `role`.
        member: String,
    },
    /// The table named by a grant or revoke is not registered (`42P01`).
    UnknownTable(String),
    /// The schema a bulk grant names is not published by the gateway (`3F000`).
    UnknownSchema(String),
    /// The `TEMPLATE` database does not exist (`3D000`).
    UnknownTemplate(String),
    /// A database option the gateway cannot honour (`22023`).
    InvalidOption(String),
    /// The sequence already exists (`42P07`).
    DuplicateSequence(String),
    /// `nextval` left the sequence's range (`2200H`).
    SequenceExhausted(String),
    /// `currval` was read before `nextval` ever ran (`55000`).
    SequenceNotCalled(String),
    /// The document could not be read or written.
    Persistence(String),
}

impl ObjectError {
    /// PostgreSQL SQLSTATE for this failure.
    pub fn sqlstate(&self) -> &'static str {
        match self {
            Self::DuplicateDatabase(_) => "42P04",
            Self::DuplicateRole(_) => "42710",
            Self::UnknownRole(_) => "42704",
            Self::CircularMembership { .. } => "0LP01",
            Self::UnknownTable(_) => "42P01",
            Self::UnknownSchema(_) => "3F000",
            Self::UnknownTemplate(_) => "3D000",
            Self::InvalidOption(_) => "22023",
            Self::DuplicateSequence(_) => "42P07",
            Self::SequenceExhausted(_) => "2200H",
            Self::SequenceNotCalled(_) => "55000",
            Self::Persistence(_) => "58P01",
        }
    }

    /// Human-readable detail.
    pub fn message(&self) -> String {
        match self {
            Self::DuplicateDatabase(name) => format!("database \"{name}\" already exists"),
            Self::DuplicateRole(name) => format!("role \"{name}\" already exists"),
            Self::UnknownRole(name) => format!("role \"{name}\" does not exist"),
            Self::CircularMembership { role, member } => format!("role \"{role}\" is a member of role \"{member}\""),
            Self::UnknownTable(name) => format!("relation \"{name}\" does not exist"),
            Self::UnknownSchema(name) => format!("schema \"{name}\" does not exist"),
            Self::UnknownTemplate(name) => format!("template database \"{name}\" does not exist"),
            Self::InvalidOption(detail) => detail.clone(),
            Self::DuplicateSequence(name) => format!("relation \"{name}\" already exists"),
            Self::SequenceExhausted(name) => format!("nextval: reached the maximum or minimum value of sequence \"{name}\""),
            Self::SequenceNotCalled(name) => format!("currval of sequence \"{name}\" is not yet defined in this session"),
            Self::Persistence(detail) => detail.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectDocument {
    contract: String,
    revision: u32,
    databases: Vec<PgDatabase>,
    roles: Vec<PgRole>,
    #[serde(default)]
    memberships: Vec<PgMembership>,
    #[serde(default)]
    sequences: Vec<PgSequence>,
    grants: Vec<PgGrant>,
}

impl ObjectDocument {
    fn empty() -> Self {
        Self {
            contract: OBJECTS_CONTRACT.to_string(),
            revision: 1,
            // The bootstrap database and role every gateway starts from.
            databases: vec![PgDatabase::new("postgres")],
            roles: vec![PgRole { name: "postgres".to_string(), superuser: true, createdb: true, login: true, inherit: true, password: None }],
            memberships: Vec::new(),
            sequences: Vec::new(),
            grants: Vec::new(),
        }
    }
}

/// Cluster-wide SQL object catalog.
#[derive(Debug)]
pub struct PgObjectCatalog {
    state: Mutex<ObjectDocument>,
    path: Option<PathBuf>,
}

impl PgObjectCatalog {
    /// Creates an in-memory catalog seeded with `postgres`.
    pub fn new_in_memory() -> Self {
        Self { state: Mutex::new(ObjectDocument::empty()), path: None }
    }

    /// Opens the catalog document at `path`, creating the seeded state when the
    /// file is absent.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ObjectError> {
        let path = path.into();
        let state = if path.exists() {
            let source = fs::read_to_string(&path)
                .map_err(|error| ObjectError::Persistence(format!("cannot read SQL objects `{}`: {error}", path.display())))?;
            let document: ObjectDocument = serde_json::from_str(&source)
                .map_err(|error| ObjectError::Persistence(format!("cannot parse SQL objects `{}`: {error}", path.display())))?;
            if document.contract != OBJECTS_CONTRACT {
                return Err(ObjectError::Persistence(format!(
                    "unsupported SQL object contract `{}`, expected `{OBJECTS_CONTRACT}`",
                    document.contract
                )));
            }
            document
        } else {
            ObjectDocument::empty()
        };
        let catalog = Self { state: Mutex::new(state), path: Some(path) };
        catalog.save(&catalog.lock())?;
        Ok(catalog)
    }

    /// Registered database names.
    pub fn databases(&self) -> Vec<String> {
        self.lock().databases.iter().map(|database| database.name.clone()).collect()
    }

    /// One database record by name.
    pub fn database(&self, name: &str) -> Option<PgDatabase> {
        self.lock().databases.iter().find(|database| database.name == name).cloned()
    }

    /// Registered roles.
    pub fn roles(&self) -> Vec<PgRole> {
        self.lock().roles.clone()
    }

    /// One role by name.
    pub fn role(&self, name: &str) -> Option<PgRole> {
        self.lock().roles.iter().find(|role| role.name == name).cloned()
    }

    /// Registers a database with its options. Returns `false` when
    /// `IF NOT EXISTS` matched.
    pub fn create_database(&self, database: PgDatabase, if_not_exists: bool) -> Result<bool, ObjectError> {
        let mut state = self.lock();
        if state.databases.iter().any(|existing| existing.name == database.name) {
            if if_not_exists {
                return Ok(false);
            }
            return Err(ObjectError::DuplicateDatabase(database.name));
        }
        let mut next = state.clone();
        next.databases.push(database);
        next.revision = next.revision.saturating_add(1);
        self.save(&next)?;
        *state = next;
        Ok(true)
    }

    /// Registers a role. Returns `false` when it already exists.
    pub fn create_role(&self, role: PgRole, if_not_exists: bool) -> Result<bool, ObjectError> {
        let mut state = self.lock();
        if state.roles.iter().any(|existing| existing.name == role.name) {
            if if_not_exists {
                return Ok(false);
            }
            return Err(ObjectError::DuplicateRole(role.name));
        }
        let mut next = state.clone();
        next.roles.push(role);
        next.revision = next.revision.saturating_add(1);
        self.save(&next)?;
        *state = next;
        Ok(true)
    }

    /// Adds privileges to a role. `table_exists` validates the relation name
    /// against the live schema so a grant to a missing table is `42P01`.
    pub fn grant(&self, role: &str, table: &str, privileges: &[PgPrivilege], table_exists: impl Fn(&str) -> bool) -> Result<(), ObjectError> {
        let mut state = self.lock();
        if !state.roles.iter().any(|existing| existing.name == role) {
            return Err(ObjectError::UnknownRole(role.to_string()));
        }
        if !table_exists(table) {
            return Err(ObjectError::UnknownTable(table.to_string()));
        }
        let mut next = state.clone();
        for privilege in privileges {
            let grant = PgGrant { role: role.to_string(), table: table.to_string(), privilege: *privilege };
            if !next.grants.contains(&grant) {
                next.grants.push(grant);
            }
        }
        next.grants.sort_by(|left, right| (&left.role, &left.table, left.privilege).cmp(&(&right.role, &right.table, right.privilege)));
        next.revision = next.revision.saturating_add(1);
        self.save(&next)?;
        *state = next;
        Ok(())
    }

    /// Adds one privilege set to `role` on every named table in one save.
    ///
    /// This is `GRANT ... ON ALL TABLES IN SCHEMA`'s expansion: the caller
    /// supplies the tables the statement must cover, so a later `CREATE TABLE`
    /// is not covered — PostgreSQL's bulk grant only touches existing
    /// relations, and `ALTER DEFAULT PRIVILEGES` is what changes the future.
    pub fn grant_tables(&self, role: &str, tables: &[String], privileges: &[PgPrivilege]) -> Result<(), ObjectError> {
        let mut state = self.lock();
        if !state.roles.iter().any(|existing| existing.name == role) {
            return Err(ObjectError::UnknownRole(role.to_string()));
        }
        let mut next = state.clone();
        for table in tables {
            for privilege in privileges {
                let grant = PgGrant { role: role.to_string(), table: table.clone(), privilege: *privilege };
                if !next.grants.contains(&grant) {
                    next.grants.push(grant);
                }
            }
        }
        next.grants.sort_by(|left, right| (&left.role, &left.table, left.privilege).cmp(&(&right.role, &right.table, right.privilege)));
        next.revision = next.revision.saturating_add(1);
        self.save(&next)?;
        *state = next;
        Ok(())
    }

    /// Removes one privilege set from `role` on every named table in one save.
    pub fn revoke_tables(&self, role: &str, tables: &[String], privileges: &[PgPrivilege]) -> Result<(), ObjectError> {
        let mut state = self.lock();
        if !state.roles.iter().any(|existing| existing.name == role) {
            return Err(ObjectError::UnknownRole(role.to_string()));
        }
        let mut next = state.clone();
        next.grants.retain(|grant| grant.role != role || !tables.contains(&grant.table) || !privileges.contains(&grant.privilege));
        next.revision = next.revision.saturating_add(1);
        self.save(&next)?;
        *state = next;
        Ok(())
    }

    /// Removes privileges from a role.
    pub fn revoke(&self, role: &str, table: &str, privileges: &[PgPrivilege], table_exists: impl Fn(&str) -> bool) -> Result<(), ObjectError> {
        let mut state = self.lock();
        if !state.roles.iter().any(|existing| existing.name == role) {
            return Err(ObjectError::UnknownRole(role.to_string()));
        }
        if !table_exists(table) {
            return Err(ObjectError::UnknownTable(table.to_string()));
        }
        let mut next = state.clone();
        next.grants.retain(|grant| !(grant.role == role && grant.table == table && privileges.contains(&grant.privilege)));
        next.revision = next.revision.saturating_add(1);
        self.save(&next)?;
        *state = next;
        Ok(())
    }

    /// Whether `role` holds `privilege` on `table`.
    ///
    /// A role the catalog does not know, and a superuser, pass every check:
    /// authentication-only deployments never created a role, so enforcing a
    /// grant table for them would reject every statement.
    ///
    /// A role with `INHERIT` also holds what its granted roles hold, which is
    /// PostgreSQL's `pg_has_role(new_role, role, 'USAGE')` rule the planner's
    /// privilege check relies on.
    pub fn has_privilege(&self, role: &str, table: &str, privilege: PgPrivilege) -> bool {
        let state = self.lock();
        if !state.roles.iter().any(|existing| existing.name == role) {
            return true;
        }
        let effective = effective_roles(&state, role);
        if effective.iter().any(|name| state.roles.iter().any(|entry| &entry.name == name && entry.superuser)) {
            return true;
        }
        state
            .grants
            .iter()
            .any(|grant| effective.iter().any(|name| *name == grant.role) && grant.table == table && grant.privilege == privilege)
    }

    /// Registers one membership edge (`member` becomes a member of `role`).
    ///
    /// Returns `false` when the edge already existed, which PostgreSQL reports
    /// as a notice rather than an error. A cycle is rejected with `0LP01`.
    pub fn grant_membership(&self, role: &str, member: &str) -> Result<bool, ObjectError> {
        let mut state = self.lock();
        for name in [role, member] {
            if !state.roles.iter().any(|existing| existing.name == name) {
                return Err(ObjectError::UnknownRole(name.to_string()));
            }
        }
        if role == member || reachable_roles(&state, role).iter().any(|name| name == member) {
            return Err(ObjectError::CircularMembership { role: role.to_string(), member: member.to_string() });
        }
        let edge = PgMembership { role: role.to_string(), member: member.to_string() };
        if state.memberships.contains(&edge) {
            return Ok(false);
        }
        let mut next = state.clone();
        next.memberships.push(edge);
        next.memberships.sort();
        next.revision = next.revision.saturating_add(1);
        self.save(&next)?;
        *state = next;
        Ok(true)
    }

    /// Removes one membership edge. Returns `false` when it was not present,
    /// which PostgreSQL reports as a warning rather than an error.
    pub fn revoke_membership(&self, role: &str, member: &str) -> Result<bool, ObjectError> {
        let mut state = self.lock();
        for name in [role, member] {
            if !state.roles.iter().any(|existing| existing.name == name) {
                return Err(ObjectError::UnknownRole(name.to_string()));
            }
        }
        let before = state.memberships.len();
        let mut next = state.clone();
        next.memberships.retain(|edge| !(edge.role == role && edge.member == member));
        if next.memberships.len() == before {
            return Ok(false);
        }
        next.revision = next.revision.saturating_add(1);
        self.save(&next)?;
        *state = next;
        Ok(true)
    }

    /// Whether `member` is `role`, or reaches it through any edge. Membership
    /// exists regardless of either role's `INHERIT` flag.
    pub fn is_member_of(&self, member: &str, role: &str) -> bool {
        let state = self.lock();
        reachable_roles(&state, member).iter().any(|name| name == role)
    }

    /// One membership edge, when it exists.
    pub fn memberships(&self) -> Vec<PgMembership> {
        self.lock().memberships.clone()
    }

    /// Registered sequences.
    pub fn sequences(&self) -> Vec<PgSequence> {
        self.lock().sequences.clone()
    }

    /// One sequence by name.
    pub fn sequence(&self, name: &str) -> Option<PgSequence> {
        self.lock().sequences.iter().find(|sequence| sequence.name == name).cloned()
    }

    /// Registers a sequence. Returns `false` when `IF NOT EXISTS` matched.
    pub fn create_sequence(&self, sequence: PgSequence, if_not_exists: bool) -> Result<bool, ObjectError> {
        let mut state = self.lock();
        if state.sequences.iter().any(|existing| existing.name == sequence.name) {
            if if_not_exists {
                return Ok(false);
            }
            return Err(ObjectError::DuplicateSequence(sequence.name));
        }
        let mut next = state.clone();
        next.sequences.push(sequence);
        next.sequences.sort_by(|left, right| left.name.cmp(&right.name));
        next.revision = next.revision.saturating_add(1);
        self.save(&next)?;
        *state = next;
        Ok(true)
    }

    /// Applies one `ALTER SEQUENCE` to a registered sequence.
    pub fn alter_sequence(&self, name: &str, alteration: &PgSequenceAlteration) -> Result<(), ObjectError> {
        let mut state = self.lock();
        let mut next = state.clone();
        let sequence =
            next.sequences.iter_mut().find(|sequence| sequence.name == name).ok_or_else(|| ObjectError::UnknownTable(name.to_string()))?;
        if let Some(increment) = alteration.increment {
            sequence.increment = increment;
        }
        if let Some(min_value) = alteration.min_value {
            sequence.min_value = min_value.unwrap_or(if sequence.increment > 0 { 1 } else { i64::MIN });
        }
        if let Some(max_value) = alteration.max_value {
            sequence.max_value = max_value.unwrap_or(if sequence.increment > 0 { i64::MAX } else { -1 });
        }
        if let Some(cycle) = alteration.cycle {
            sequence.cycle = cycle;
        }
        if let Some(restart) = alteration.restart {
            sequence.last_value = restart.unwrap_or(sequence.start);
            sequence.is_called = false;
        }
        next.revision = next.revision.saturating_add(1);
        self.save(&next)?;
        *state = next;
        Ok(())
    }

    /// Drops a sequence. A missing name is `42P01` unless `IF EXISTS` matched.
    pub fn drop_sequence(&self, name: &str, if_exists: bool) -> Result<bool, ObjectError> {
        let mut state = self.lock();
        let before = state.sequences.len();
        let mut next = state.clone();
        next.sequences.retain(|sequence| sequence.name != name);
        if next.sequences.len() == before {
            if if_exists {
                return Ok(false);
            }
            return Err(ObjectError::UnknownTable(name.to_string()));
        }
        next.revision = next.revision.saturating_add(1);
        self.save(&next)?;
        *state = next;
        Ok(true)
    }

    /// Hands out the next value of `name` and persists the advance.
    pub fn nextval(&self, name: &str) -> Result<i64, ObjectError> {
        let mut state = self.lock();
        let mut next = state.clone();
        let sequence =
            next.sequences.iter_mut().find(|sequence| sequence.name == name).ok_or_else(|| ObjectError::UnknownTable(name.to_string()))?;
        let value = sequence.advance()?;
        next.revision = next.revision.saturating_add(1);
        self.save(&next)?;
        *state = next;
        Ok(value)
    }

    /// Last value `nextval` handed out for `name`.
    pub fn currval(&self, name: &str) -> Result<i64, ObjectError> {
        let state = self.lock();
        let sequence =
            state.sequences.iter().find(|sequence| sequence.name == name).ok_or_else(|| ObjectError::UnknownTable(name.to_string()))?;
        if !sequence.is_called {
            return Err(ObjectError::SequenceNotCalled(name.to_string()));
        }
        Ok(sequence.last_value)
    }

    /// Sets the sequence's position for the next `nextval` call.
    pub fn setval(&self, name: &str, value: i64, is_called: bool) -> Result<i64, ObjectError> {
        let mut state = self.lock();
        let mut next = state.clone();
        let sequence =
            next.sequences.iter_mut().find(|sequence| sequence.name == name).ok_or_else(|| ObjectError::UnknownTable(name.to_string()))?;
        sequence.last_value = value;
        sequence.is_called = is_called;
        next.revision = next.revision.saturating_add(1);
        self.save(&next)?;
        *state = next;
        Ok(value)
    }

    /// Whether `role` exists and is a superuser.
    pub fn is_superuser(&self, role: &str) -> bool {
        self.lock().roles.iter().any(|entry| entry.name == role && entry.superuser)
    }

    fn lock(&self) -> MutexGuard<'_, ObjectDocument> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn save(&self, state: &ObjectDocument) -> Result<(), ObjectError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        write_document(path, state)
    }
}

fn write_document(path: &Path, state: &ObjectDocument) -> Result<(), ObjectError> {
    let json = serde_json::to_string_pretty(state).map_err(|error| ObjectError::Persistence(format!("cannot encode SQL objects: {error}")))?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|error| ObjectError::Persistence(format!("cannot create SQL object directory `{}`: {error}", parent.display())))?;
        }
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    fs::write(&temporary, json)
        .map_err(|error| ObjectError::Persistence(format!("cannot write SQL objects `{}`: {error}", temporary.display())))?;
    if fs::rename(&temporary, path).is_err() {
        if path.exists() {
            fs::remove_file(path)
                .map_err(|error| ObjectError::Persistence(format!("cannot replace SQL objects `{}`: {error}", path.display())))?;
        }
        fs::rename(&temporary, path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            ObjectError::Persistence(format!("cannot replace SQL objects `{}`: {error}", path.display()))
        })?;
    }
    Ok(())
}

/// Distinct role names, in catalog order.
pub fn role_names(catalog: &PgObjectCatalog) -> BTreeSet<String> {
    catalog.roles().into_iter().map(|role| role.name).collect()
}

/// Every role `role` reaches through membership edges, itself included.
///
/// This is PostgreSQL's `pg_has_role` answer: membership exists whether or not
/// the role inherits, so cycle detection and `is_member_of` use it directly.
/// The closure itself is the shared reachability core over the membership
/// edges; a role with no outgoing edge simply reaches itself.
fn reachable_roles(state: &ObjectDocument, role: &str) -> BTreeSet<String> {
    let mut graph = boost_collection::MemoryGraph::<String>::new(true);
    for edge in &state.memberships {
        graph.add_vertex(edge.member.clone()).ok();
        graph.add_vertex(edge.role.clone()).ok();
    }
    for edge in &state.memberships {
        graph.add_edge(edge.member.clone(), edge.role.clone(), ()).expect("endpoints were added");
    }
    match boost_math::graph_theory::reachable_from(&graph, &role.to_string()) {
        Ok(reached) => reached.into_iter().collect(),
        // `role` never appears as a member: the closure is just itself.
        Err(_) => std::iter::once(role.to_string()).collect(),
    }
}

/// The roles whose privileges `role` automatically uses: itself, and every
/// reachable role when it is `INHERIT`. A `NOINHERIT` role keeps the edges but
/// does not use them for privilege checks, exactly like PostgreSQL.
fn effective_roles(state: &ObjectDocument, role: &str) -> BTreeSet<String> {
    if state.roles.iter().find(|entry| entry.name == role).is_none_or(|entry| entry.inherit) {
        return reachable_roles(state, role);
    }
    let mut names = BTreeSet::new();
    names.insert(role.to_string());
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_databases_and_roles_survive_a_reopen() {
        let path = std::env::temp_dir().join(format!("pg-objects-{}.json", std::process::id()));
        let _ = fs::remove_file(&path);
        {
            let catalog = PgObjectCatalog::open(&path).unwrap();
            assert!(catalog.create_database(PgDatabase::new("app"), false).unwrap());
            assert!(!catalog.create_database(PgDatabase::new("app"), true).unwrap());
            assert_eq!(catalog.create_database(PgDatabase::new("app"), false).unwrap_err().sqlstate(), "42P04");
            catalog
                .create_role(
                    PgRole { name: "reader".into(), superuser: false, createdb: false, login: true, inherit: true, password: None },
                    false,
                )
                .unwrap();
            catalog.grant("reader", "orders", &[PgPrivilege::Select], |table| table == "orders").unwrap();
            assert!(catalog.has_privilege("reader", "orders", PgPrivilege::Select));
            assert!(!catalog.has_privilege("reader", "orders", PgPrivilege::Insert));
        }
        let reopened = PgObjectCatalog::open(&path).unwrap();
        assert_eq!(reopened.databases(), vec!["postgres".to_string(), "app".to_string()]);
        assert_eq!(reopened.role("reader").map(|role| role.login), Some(true));
        assert!(reopened.has_privilege("reader", "orders", PgPrivilege::Select));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_role_password_is_stored_as_a_verifier_and_survives_a_reopen() {
        let path = std::env::temp_dir().join(format!("pg-role-password-{}.json", std::process::id()));
        let _ = fs::remove_file(&path);
        {
            let catalog = PgObjectCatalog::open(&path).unwrap();
            let password = PgPassword::derive("r0le", "app").unwrap();
            assert!(password.verifies_cleartext("r0le", "app"));
            assert!(!password.verifies_cleartext("wrong", "app"));
            let (verifier, salt) = password.scram().expect("verifier decodes");
            assert_eq!(salt.len(), 16);
            assert_eq!(verifier.stored_key.len(), 32);
            catalog
                .create_role(
                    PgRole { name: "app".into(), superuser: false, createdb: false, login: true, inherit: true, password: Some(password) },
                    false,
                )
                .unwrap();
        }
        let reopened = PgObjectCatalog::open(&path).unwrap();
        let password = reopened.role("app").and_then(|role| role.password).expect("the verifier survived the reopen");
        assert!(password.verifies_cleartext("r0le", "app"));
        let source = fs::read_to_string(&path).unwrap();
        assert!(!source.contains("r0le"), "the document must never hold the plaintext");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn membership_edges_grant_privileges_transitively_and_reject_cycles() {
        let catalog = PgObjectCatalog::new_in_memory();
        let role =
            |name: &str, inherit: bool| PgRole { name: name.into(), superuser: false, createdb: false, login: true, inherit, password: None };
        catalog.create_role(role("reader", true), false).unwrap();
        catalog.create_role(role("app", true), false).unwrap();
        catalog.create_role(role("worker", false), false).unwrap();
        catalog.grant("reader", "orders", &[PgPrivilege::Select], |_| true).unwrap();

        assert!(catalog.grant_membership("reader", "app").unwrap());
        assert!(!catalog.grant_membership("reader", "app").unwrap(), "a repeated edge is a notice");
        assert!(catalog.is_member_of("app", "reader"));
        assert!(catalog.has_privilege("app", "orders", PgPrivilege::Select), "a member inherits the grant");
        assert!(!catalog.has_privilege("app", "orders", PgPrivilege::Delete));

        // Transitivity: `app` is a member of `reader`, so making `reader` a
        // member of `app` would close a cycle.
        let cycle = catalog.grant_membership("app", "reader").unwrap_err();
        assert_eq!(cycle.sqlstate(), "0LP01");
        assert_eq!(catalog.grant_membership("reader", "reader").unwrap_err().sqlstate(), "0LP01");
        assert_eq!(catalog.grant_membership("reader", "ghost").unwrap_err().sqlstate(), "42704");

        // `NOINHERIT` keeps the edge but not automatic privilege use.
        catalog.grant_membership("reader", "worker").unwrap();
        assert!(catalog.is_member_of("worker", "reader"));
        assert!(!catalog.has_privilege("worker", "orders", PgPrivilege::Select));

        assert!(catalog.revoke_membership("reader", "app").unwrap());
        assert!(!catalog.revoke_membership("reader", "app").unwrap(), "a missing edge is a warning");
        assert!(!catalog.has_privilege("app", "orders", PgPrivilege::Select));
    }

    #[test]
    fn grant_errors_carry_their_sqlstates() {
        let catalog = PgObjectCatalog::new_in_memory();
        assert_eq!(catalog.grant("ghost", "orders", &[PgPrivilege::Select], |_| true).unwrap_err().sqlstate(), "42704");
        catalog
            .create_role(PgRole { name: "reader".into(), superuser: false, createdb: false, login: true, inherit: true, password: None }, false)
            .unwrap();
        assert_eq!(catalog.grant("reader", "missing", &[PgPrivilege::Select], |_| false).unwrap_err().sqlstate(), "42P01");
        assert!(catalog.has_privilege("unknown-role", "missing", PgPrivilege::Delete));
        assert!(catalog.is_superuser("postgres"));
    }
}
