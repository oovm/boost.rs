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
    /// The table named by a grant or revoke is not registered (`42P01`).
    UnknownTable(String),
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
            Self::UnknownTable(_) => "42P01",
            Self::Persistence(_) => "58P01",
        }
    }

    /// Human-readable detail.
    pub fn message(&self) -> String {
        match self {
            Self::DuplicateDatabase(name) => format!("database \"{name}\" already exists"),
            Self::DuplicateRole(name) => format!("role \"{name}\" already exists"),
            Self::UnknownRole(name) => format!("role \"{name}\" does not exist"),
            Self::UnknownTable(name) => format!("relation \"{name}\" does not exist"),
            Self::Persistence(detail) => detail.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectDocument {
    contract: String,
    revision: u32,
    databases: Vec<String>,
    roles: Vec<PgRole>,
    grants: Vec<PgGrant>,
}

impl ObjectDocument {
    fn empty() -> Self {
        Self {
            contract: OBJECTS_CONTRACT.to_string(),
            revision: 1,
            // The bootstrap database and role every gateway starts from.
            databases: vec!["postgres".to_string()],
            roles: vec![PgRole { name: "postgres".to_string(), superuser: true, createdb: true, login: true }],
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
        }
        else {
            ObjectDocument::empty()
        };
        let catalog = Self { state: Mutex::new(state), path: Some(path) };
        catalog.save(&catalog.lock())?;
        Ok(catalog)
    }

    /// Registered database names.
    pub fn databases(&self) -> Vec<String> {
        self.lock().databases.clone()
    }

    /// Registered roles.
    pub fn roles(&self) -> Vec<PgRole> {
        self.lock().roles.clone()
    }

    /// One role by name.
    pub fn role(&self, name: &str) -> Option<PgRole> {
        self.lock().roles.iter().find(|role| role.name == name).cloned()
    }

    /// Registers a database. Returns `false` when `IF NOT EXISTS` matched.
    pub fn create_database(&self, name: &str, if_not_exists: bool) -> Result<bool, ObjectError> {
        let mut state = self.lock();
        if state.databases.iter().any(|database| database == name) {
            if if_not_exists {
                return Ok(false);
            }
            return Err(ObjectError::DuplicateDatabase(name.to_string()));
        }
        let mut next = state.clone();
        next.databases.push(name.to_string());
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
    pub fn grant(
        &self,
        role: &str,
        table: &str,
        privileges: &[PgPrivilege],
        table_exists: impl Fn(&str) -> bool,
    ) -> Result<(), ObjectError> {
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

    /// Removes privileges from a role.
    pub fn revoke(
        &self,
        role: &str,
        table: &str,
        privileges: &[PgPrivilege],
        table_exists: impl Fn(&str) -> bool,
    ) -> Result<(), ObjectError> {
        let mut state = self.lock();
        if !state.roles.iter().any(|existing| existing.name == role) {
            return Err(ObjectError::UnknownRole(role.to_string()));
        }
        if !table_exists(table) {
            return Err(ObjectError::UnknownTable(table.to_string()));
        }
        let mut next = state.clone();
        next.grants
            .retain(|grant| !(grant.role == role && grant.table == table && privileges.contains(&grant.privilege)));
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
    pub fn has_privilege(&self, role: &str, table: &str, privilege: PgPrivilege) -> bool {
        let state = self.lock();
        let Some(entry) = state.roles.iter().find(|existing| existing.name == role) else {
            return true;
        };
        if entry.superuser {
            return true;
        }
        state.grants.iter().any(|grant| grant.role == role && grant.table == table && grant.privilege == privilege)
    }

    /// Whether `role` exists and is a superuser.
    pub fn is_superuser(&self, role: &str) -> bool {
        self.lock().roles.iter().any(|entry| entry.name == role && entry.superuser)
    }

    fn lock(&self) -> MutexGuard<'_, ObjectDocument> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn save(&self, state: &ObjectDocument) -> Result<(), ObjectError> {
        let Some(path) = &self.path
        else {
            return Ok(());
        };
        write_document(path, state)
    }
}

fn write_document(path: &Path, state: &ObjectDocument) -> Result<(), ObjectError> {
    let json = serde_json::to_string_pretty(state)
        .map_err(|error| ObjectError::Persistence(format!("cannot encode SQL objects: {error}")))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_databases_and_roles_survive_a_reopen() {
        let path = std::env::temp_dir().join(format!("pg-objects-{}.json", std::process::id()));
        let _ = fs::remove_file(&path);
        {
            let catalog = PgObjectCatalog::open(&path).unwrap();
            assert!(catalog.create_database("app", false).unwrap());
            assert!(!catalog.create_database("app", true).unwrap());
            assert_eq!(catalog.create_database("app", false).unwrap_err().sqlstate(), "42P04");
            catalog
                .create_role(PgRole { name: "reader".into(), superuser: false, createdb: false, login: true }, false)
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
    fn grant_errors_carry_their_sqlstates() {
        let catalog = PgObjectCatalog::new_in_memory();
        assert_eq!(catalog.grant("ghost", "orders", &[PgPrivilege::Select], |_| true).unwrap_err().sqlstate(), "42704");
        catalog.create_role(PgRole { name: "reader".into(), superuser: false, createdb: false, login: true }, false).unwrap();
        assert_eq!(catalog.grant("reader", "missing", &[PgPrivilege::Select], |_| false).unwrap_err().sqlstate(), "42P01");
        assert!(catalog.has_privilege("unknown-role", "missing", PgPrivilege::Delete));
        assert!(catalog.is_superuser("postgres"));
    }
}
