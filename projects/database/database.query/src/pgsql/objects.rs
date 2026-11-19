//! `CREATE DATABASE`, `CREATE ROLE`, `GRANT` and `REVOKE`.
//!
//! These statements change cluster-wide objects rather than a table, so they
//! bypass the DDL subset the schema registry owns. Everything outside the
//! implemented subset is rejected with `0A000` and a reason: the gateway never
//! accepts a clause it would then ignore.

use database_storage::pgsql::{PgPrivilege, PgRole, PgSequence, PgSequenceAlteration};

use crate::sql::{SqlErrorKind, SqlFrontendError};

/// One cluster-object statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgObjectStatement {
    /// `CREATE DATABASE [IF NOT EXISTS] name`.
    CreateDatabase {
        /// Database name.
        name: String,
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
    },
    /// `CREATE ROLE|USER name [WITH ...]`.
    CreateRole {
        /// Role definition.
        role: PgRole,
        /// Plaintext password the statement set, `None` when it named none.
        /// Execution derives the stored verifier, so the password never leaves
        /// this one call.
        password: Option<String>,
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
    },
    /// `GRANT privilege[, ...] ON [TABLE] name TO role[, ...]`.
    Grant {
        /// Privileges to add.
        privileges: Vec<PgPrivilege>,
        /// Target table.
        table: String,
        /// Grantee roles.
        roles: Vec<String>,
    },
    /// `REVOKE privilege[, ...] ON [TABLE] name FROM role[, ...]`.
    Revoke {
        /// Privileges to remove.
        privileges: Vec<PgPrivilege>,
        /// Target table.
        table: String,
        /// Roles the privileges are removed from.
        roles: Vec<String>,
    },
    /// `GRANT role[, ...] TO role[, ...]`.
    GrantMembership {
        /// Roles whose privileges are inherited.
        roles: Vec<String>,
        /// Roles that become members.
        members: Vec<String>,
    },
    /// `REVOKE role[, ...] FROM role[, ...]`.
    RevokeMembership {
        /// Roles whose privileges were inherited.
        roles: Vec<String>,
        /// Roles that stop being members.
        members: Vec<String>,
    },
    /// `GRANT privilege[, ...] ON ALL TABLES IN SCHEMA name TO role[, ...]`.
    GrantAllTables {
        /// Schema the grant covers.
        schema: String,
        /// Privileges to add.
        privileges: Vec<PgPrivilege>,
        /// Grantee roles.
        roles: Vec<String>,
    },
    /// `REVOKE privilege[, ...] ON ALL TABLES IN SCHEMA name FROM role[, ...]`.
    RevokeAllTables {
        /// Schema the revoke covers.
        schema: String,
        /// Privileges to remove.
        privileges: Vec<PgPrivilege>,
        /// Roles the privileges are removed from.
        roles: Vec<String>,
    },
    /// `CREATE SEQUENCE [IF NOT EXISTS] name [options]`.
    CreateSequence {
        /// Sequence definition and initial state.
        sequence: PgSequence,
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
    },
    /// `ALTER SEQUENCE name [options]`.
    AlterSequence {
        /// Sequence to change.
        name: String,
        /// Fields the statement sets.
        alteration: PgSequenceAlteration,
    },
    /// `DROP SEQUENCE [IF EXISTS] name[, ...]`.
    DropSequence {
        /// Sequences to remove.
        names: Vec<String>,
        /// Whether `IF EXISTS` was written.
        if_exists: bool,
    },
}

/// Parses one cluster-object statement, or `Ok(None)` when the text is not one.
pub fn parse_pg_object_statement(source: &str) -> Result<Option<PgObjectStatement>, SqlFrontendError> {
    let trimmed = source.trim().trim_end_matches(';').trim();
    let Some(keyword) = first_word(trimmed)
    else {
        return Ok(None);
    };
    let tail = trimmed[keyword.len()..].trim_start();
    match keyword.to_ascii_uppercase().as_str() {
        "CREATE" => parse_create(tail),
        "ALTER" => {
            // `ALTER DEFAULT PRIVILEGES` would change what *future* objects
            // grant. `GRANT ... ON ALL TABLES IN SCHEMA` only covers existing
            // relations, so this stays an explicit rejection instead of
            // pretending the bulk grant reaches later tables.
            if let Some(after) = strip_keyword(tail, "DEFAULT") {
                strip_keyword(after, "PRIVILEGES").ok_or_else(|| syntax("`ALTER DEFAULT` requires `PRIVILEGES`"))?;
                return Err(unsupported(
                    "`ALTER DEFAULT PRIVILEGES` is not supported by the cluster SQL gateway: `GRANT ... ON ALL TABLES IN SCHEMA` covers the tables that exist when it runs, exactly like PostgreSQL, and nothing applies to tables created later",
                ));
            }
            if let Some(after) = strip_keyword(tail, "SEQUENCE") {
                return parse_alter_sequence(after).map(Some);
            }
            // Other `ALTER` statements belong to the schema registry.
            Ok(None)
        }
        "DROP" => {
            let Some(kind) = first_word(tail)
            else {
                return Ok(None);
            };
            if kind.eq_ignore_ascii_case("SEQUENCE") {
                return parse_drop_sequence(&tail[kind.len()..]).map(Some);
            }
            // `DROP TABLE` / `DROP INDEX` belong to the schema registry.
            Ok(None)
        }
        "GRANT" => parse_grant(tail, true).map(Some),
        "REVOKE" => parse_grant(tail, false).map(Some),
        _ => Ok(None),
    }
}

fn parse_create(tail: &str) -> Result<Option<PgObjectStatement>, SqlFrontendError> {
    let Some(keyword) = first_word(tail)
    else {
        return Err(unsupported("`CREATE` requires an object kind"));
    };
    let rest = tail[keyword.len()..].trim_start();
    match keyword.to_ascii_uppercase().as_str() {
        "DATABASE" => {
            let (name, rest) = take_identifier(rest)?;
            if !identification_tail_is_blank(rest) {
                return Err(unsupported(
                    "`CREATE DATABASE` options are not supported by the cluster SQL gateway, only the database name is recorded",
                ));
            }
            Ok(Some(PgObjectStatement::CreateDatabase { name, if_not_exists: false }))
        }
        "ROLE" | "USER" => {
            let login_default = keyword.eq_ignore_ascii_case("USER");
            parse_create_role(rest, login_default).map(Some)
        }
        // Table, index and temporary-table DDL belong to the schema registry.
        "TABLE" | "TEMP" | "TEMPORARY" | "INDEX" | "UNIQUE" => Ok(None),
        "SEQUENCE" => parse_create_sequence(rest).map(Some),
        _ => Err(unsupported(format!(
            "`CREATE {keyword}` is not supported by the cluster SQL gateway, only `DATABASE`, `ROLE` and `USER` are"
        ))),
    }
}

fn identification_tail_is_blank(rest: &str) -> bool {
    rest.trim().is_empty()
}

fn parse_create_role(tail: &str, login_default: bool) -> Result<PgObjectStatement, SqlFrontendError> {
    let (mut rest, if_not_exists) = if_not_exists(tail);
    let (name, remaining) = take_identifier(rest)?;
    rest = remaining.trim_start();
    let mut role = PgRole { name, superuser: false, createdb: false, login: login_default, inherit: true, password: None };
    let mut password = None;
    if let Some(after) = strip_keyword(rest, "WITH") {
        rest = after.trim_start();
    }
    while !rest.trim().is_empty() {
        rest = rest.trim_start();
        let Some(word) = first_word(rest)
        else {
            return Err(syntax("unexpected token in `CREATE ROLE`"));
        };
        let after = rest[word.len()..].trim_start();
        match word.to_ascii_uppercase().as_str() {
            "LOGIN" => role.login = true,
            "NOLOGIN" => role.login = false,
            "SUPERUSER" => role.superuser = true,
            "NOSUPERUSER" => role.superuser = false,
            "CREATEDB" => role.createdb = true,
            "NOCREATEDB" => role.createdb = false,
            "INHERIT" => role.inherit = true,
            "NOINHERIT" => role.inherit = false,
            "REPLICATION" | "NOREPLICATION" | "BYPASSRLS" | "NOBYPASSRLS" => {}
            "PASSWORD" => {
                let (value, remaining) = take_password(after)?;
                password = value;
                rest = remaining;
                continue;
            }
            other => {
                return Err(unsupported(format!(
                    "`CREATE ROLE ... {other}` is not supported by the cluster SQL gateway"
                )));
            }
        }
        rest = after;
    }
    Ok(PgObjectStatement::CreateRole { role, password, if_not_exists })
}

/// Reads the value of a `PASSWORD` clause: a string literal or `NULL`.
fn take_password(source: &str) -> Result<(Option<String>, &str), SqlFrontendError> {
    let trimmed = source.trim_start();
    if let Some(rest) = strip_keyword(trimmed, "NULL") {
        return Ok((None, rest));
    }
    let Some(inner) = trimmed.strip_prefix('\'')
    else {
        return Err(syntax("`CREATE ROLE ... PASSWORD` requires a string literal or `NULL`"));
    };
    let mut value = String::with_capacity(inner.len());
    let mut characters = inner.char_indices();
    while let Some((index, character)) = characters.next() {
        if character != '\'' {
            value.push(character);
            continue;
        }
        // `''` is an escaped quote; a single one closes the literal.
        if inner[index + 1..].starts_with('\'') {
            value.push('\'');
            characters.next();
            continue;
        }
        return Ok((Some(value), &inner[index + 1..]));
    }
    Err(syntax("unterminated `PASSWORD` literal"))
}

fn parse_grant(tail: &str, granting: bool) -> Result<PgObjectStatement, SqlFrontendError> {
    let (mut rest, _) = if_not_exists(tail);
    // `GRANT role[, ...] TO role[, ...]` carries no `ON`, so it is membership
    // rather than a table privilege.
    if split_keyword(rest, "ON").is_none() {
        return parse_membership(rest, granting);
    }
    let upper = rest.to_ascii_uppercase();
    if upper.contains("SEQUENCES IN SCHEMA") {
        return Err(unsupported(
            "`GRANT ... ON ALL SEQUENCES IN SCHEMA` is not supported by the cluster SQL gateway, only `ALL TABLES` is",
        ));
    }
    if !granting {
        if let Some(after) = strip_keyword(rest, "GRANT") {
            // `REVOKE GRANT OPTION FOR ...`
            rest = strip_keyword(after.trim_start(), "OPTION")
                .ok_or_else(|| syntax("`REVOKE GRANT` requires `OPTION FOR`"))?
                .trim_start();
            rest = strip_keyword(rest, "FOR").ok_or_else(|| syntax("`REVOKE GRANT OPTION` requires `FOR`"))?.trim_start();
        }
    }
    let (privileges_text, after_privileges) = split_keyword(rest, "ON")
        .ok_or_else(|| syntax(if granting { "`GRANT` requires `ON <table>`" } else { "`REVOKE` requires `ON <table>`" }))?;
    let privileges = parse_privileges(&privileges_text)?;
    let object = after_privileges.trim_start();
    let upper_object = object.to_ascii_uppercase();
    if upper_object.starts_with("ALL TABLES") || upper_object.starts_with("ALL SEQUENCES") {
        return parse_bulk_grant(object, privileges, granting);
    }
    let mut rest = object;
    if let Some(after) = strip_keyword(rest, "TABLE") {
        rest = after.trim_start();
    }
    if rest.to_ascii_uppercase().starts_with("ALL TABLES") || rest.to_ascii_uppercase().starts_with("ALL SEQUENCES") {
        return Err(unsupported(
            "`GRANT ... ON ALL TABLES` is not supported by the cluster SQL gateway, name each table",
        ));
    }
    let (table, after_table) = take_identifier(rest)?;
    let mut rest = after_table.trim_start();
    if let Some(after) = rest.strip_prefix('.') {
        return Err(unsupported(format!(
            "qualified table names are not supported by the cluster SQL gateway, write `{}` alone",
            after.trim_start().split_whitespace().next().unwrap_or("table")
        )));
    }
    rest = rest.trim_start();
    let separator = if granting { "TO" } else { "FROM" };
    let Some(rest) = strip_keyword(rest, separator)
    else {
        return Err(syntax(format!("`{}` requires `{separator} <role>`", if granting { "GRANT" } else { "REVOKE" })));
    };
    let roles = split_identifiers(rest)?;
    if roles.is_empty() {
        return Err(syntax("the statement names no role"));
    }
    Ok(if granting {
        PgObjectStatement::Grant { privileges, table, roles }
    }
    else {
        PgObjectStatement::Revoke { privileges, table, roles }
    })
}

/// Parses `ALL TABLES IN SCHEMA <name> TO|FROM <roles>`.
///
/// `ALL SEQUENCES IN SCHEMA` has no privilege model in this gateway, so it stays
/// an explicit rejection instead of granting something it never checks.
fn parse_bulk_grant(
    object: &str,
    privileges: Vec<PgPrivilege>,
    granting: bool,
) -> Result<PgObjectStatement, SqlFrontendError> {
    let (kind, rest) = if let Some(rest) = strip_keyword(object, "ALL") {
        let rest = rest.trim_start();
        let kind = first_word(rest).ok_or_else(|| syntax("`ON ALL` requires `TABLES IN SCHEMA`"))?;
        (kind.to_ascii_uppercase(), rest[kind.len()..].trim_start())
    }
    else {
        return Err(syntax("expected `ALL TABLES IN SCHEMA`"));
    };
    if kind != "TABLES" {
        return Err(unsupported(format!(
            "`GRANT ... ON ALL {kind} IN SCHEMA` is not supported by the cluster SQL gateway, only `ALL TABLES` is"
        )));
    }
    let rest = strip_keyword(rest, "IN").ok_or_else(|| syntax("`ON ALL TABLES` requires `IN SCHEMA`"))?.trim_start();
    let rest = strip_keyword(rest, "SCHEMA").ok_or_else(|| syntax("`ON ALL TABLES IN` requires `SCHEMA`"))?.trim_start();
    let (schema, rest) = take_identifier(rest)?;
    let separator = if granting { "TO" } else { "FROM" };
    let rest = strip_keyword(rest.trim_start(), separator)
        .ok_or_else(|| syntax(format!("`ON ALL TABLES IN SCHEMA` requires `{separator} <role>`")))?;
    let roles = split_identifiers(rest)?;
    if roles.is_empty() {
        return Err(syntax("the statement names no role"));
    }
    Ok(if granting {
        PgObjectStatement::GrantAllTables { schema, privileges, roles }
    }
    else {
        PgObjectStatement::RevokeAllTables { schema, privileges, roles }
    })
}

/// Parses `CREATE SEQUENCE [IF NOT EXISTS] name [AS type] [INCREMENT …] …`.
fn parse_create_sequence(tail: &str) -> Result<PgObjectStatement, SqlFrontendError> {
    let (mut rest, if_not_exists) = if_not_exists(tail);
    let (name, remaining) = take_identifier(rest)?;
    rest = remaining.trim_start();
    let mut increment = 1i64;
    let mut type_bounds: (i64, i64) = (i64::MIN, i64::MAX);
    let mut min_value = None;
    let mut max_value = None;
    let mut start = None;
    let mut cycle = false;
    while !rest.trim().is_empty() {
        rest = rest.trim_start();
        let Some(word) = first_word(rest)
        else {
            return Err(syntax("unexpected token in `CREATE SEQUENCE`"));
        };
        let after = rest[word.len()..].trim_start();
        match word.to_ascii_uppercase().as_str() {
            "AS" => {
                let (type_name, remaining) = take_identifier(after)?;
                type_bounds = sequence_type_bounds(&type_name)?;
                rest = remaining;
                continue;
            }
            "INCREMENT" => {
                let (value, remaining) = take_sequence_number(after, "BY")?;
                increment = value;
                rest = remaining;
                continue;
            }
            "MINVALUE" => {
                let (value, remaining) = take_sequence_number(after, "")?;
                min_value = Some(value);
                rest = remaining;
                continue;
            }
            "MAXVALUE" => {
                let (value, remaining) = take_sequence_number(after, "")?;
                max_value = Some(value);
                rest = remaining;
                continue;
            }
            "START" => {
                let (value, remaining) = take_sequence_number(after, "WITH")?;
                start = Some(value);
                rest = remaining;
                continue;
            }
            "CACHE" => {
                let (value, remaining) = take_sequence_number(after, "")?;
                if value < 1 {
                    return Err(syntax("`CACHE` must be a positive number"));
                }
                // CACHE is a backend hint; it never changes nextval semantics.
                rest = remaining;
                continue;
            }
            "CYCLE" => cycle = true,
            "NO" => {
                let Some(flag) = first_word(after)
                else {
                    return Err(syntax("`NO` requires `MINVALUE`, `MAXVALUE` or `CYCLE`"));
                };
                let remaining = after[flag.len()..].trim_start();
                match flag.to_ascii_uppercase().as_str() {
                    "MINVALUE" => min_value = Some(if increment > 0 { 1 } else { type_bounds.0 }),
                    "MAXVALUE" => max_value = Some(if increment > 0 { type_bounds.1 } else { -1 }),
                    "CYCLE" => cycle = false,
                    other => return Err(unsupported(format!("`CREATE SEQUENCE ... NO {other}` is not supported"))),
                }
                rest = remaining;
                continue;
            }
            "OWNED" => {
                return Err(unsupported(
                    "`CREATE SEQUENCE ... OWNED BY` is not supported by the cluster SQL gateway, the sequence is independent",
                ));
            }
            other => {
                return Err(unsupported(format!(
                    "`CREATE SEQUENCE ... {other}` is not supported by the cluster SQL gateway"
                )));
            }
        }
        rest = after;
    }
    let min_value = min_value.unwrap_or(if increment > 0 { 1 } else { type_bounds.0 });
    let max_value = max_value.unwrap_or(if increment > 0 { type_bounds.1 } else { -1 });
    let start = start.unwrap_or(if increment > 0 { min_value } else { max_value });
    Ok(PgObjectStatement::CreateSequence {
        sequence: PgSequence::new(name, start, increment, min_value, max_value, cycle),
        if_not_exists,
    })
}

/// Integer bounds one `CREATE SEQUENCE ... AS type` selects.
fn sequence_type_bounds(name: &str) -> Result<(i64, i64), SqlFrontendError> {
    match name.to_ascii_uppercase().as_str() {
        "SMALLINT" | "INT2" => Ok((i16::MIN as i64, i16::MAX as i64)),
        "INTEGER" | "INT" | "INT4" => Ok((i32::MIN as i64, i32::MAX as i64)),
        "BIGINT" | "INT8" => Ok((i64::MIN, i64::MAX)),
        other => Err(unsupported(format!(
            "`CREATE SEQUENCE ... AS {other}` is not supported, use SMALLINT, INTEGER or BIGINT"
        ))),
    }
}

/// Reads one signed integer, optionally preceded by `keyword` (`BY` / `WITH`).
fn take_sequence_number<'a>(source: &'a str, keyword: &str) -> Result<(i64, &'a str), SqlFrontendError> {
    let mut rest = source.trim_start();
    if !keyword.is_empty()
        && let Some(after) = strip_keyword(rest, keyword)
    {
        rest = after.trim_start();
    }
    let end = rest
        .find(|character: char| !(character.is_ascii_digit() || character == '-' || character == '+'))
        .unwrap_or(rest.len());
    let value = rest[..end].parse::<i64>().map_err(|_| syntax("expected an integer sequence option"))?;
    Ok((value, &rest[end..]))
}

/// Parses `ALTER SEQUENCE name [INCREMENT …] [RESTART …] …`.
fn parse_alter_sequence(tail: &str) -> Result<PgObjectStatement, SqlFrontendError> {
    let (name, remaining) = take_identifier(tail.trim_start())?;
    let mut rest = remaining.trim_start();
    let mut alteration = PgSequenceAlteration::default();
    while !rest.trim().is_empty() {
        rest = rest.trim_start();
        let Some(word) = first_word(rest)
        else {
            return Err(syntax("unexpected token in `ALTER SEQUENCE`"));
        };
        let after = rest[word.len()..].trim_start();
        match word.to_ascii_uppercase().as_str() {
            "INCREMENT" => {
                let (value, remaining) = take_sequence_number(after, "BY")?;
                alteration.increment = Some(value);
                rest = remaining;
                continue;
            }
            "RESTART" => {
                let trimmed = after.trim_start();
                if trimmed.is_empty() || first_word(trimmed).is_some_and(|word| !word.eq_ignore_ascii_case("WITH")) {
                    alteration.restart = Some(None);
                    rest = trimmed;
                    continue;
                }
                let (value, remaining) = take_sequence_number(trimmed, "WITH")?;
                alteration.restart = Some(Some(value));
                rest = remaining;
                continue;
            }
            "MINVALUE" => {
                let (value, remaining) = take_sequence_number(after, "")?;
                alteration.min_value = Some(Some(value));
                rest = remaining;
                continue;
            }
            "MAXVALUE" => {
                let (value, remaining) = take_sequence_number(after, "")?;
                alteration.max_value = Some(Some(value));
                rest = remaining;
                continue;
            }
            "CACHE" => {
                let (value, remaining) = take_sequence_number(after, "")?;
                if value < 1 {
                    return Err(syntax("`CACHE` must be a positive number"));
                }
                rest = remaining;
                continue;
            }
            "CYCLE" => alteration.cycle = Some(true),
            "NO" => {
                let Some(flag) = first_word(after)
                else {
                    return Err(syntax("`NO` requires `MINVALUE`, `MAXVALUE` or `CYCLE`"));
                };
                let remaining = after[flag.len()..].trim_start();
                match flag.to_ascii_uppercase().as_str() {
                    "MINVALUE" => alteration.min_value = Some(None),
                    "MAXVALUE" => alteration.max_value = Some(None),
                    "CYCLE" => alteration.cycle = Some(false),
                    other => return Err(unsupported(format!("`ALTER SEQUENCE ... NO {other}` is not supported"))),
                }
                rest = remaining;
                continue;
            }
            "OWNED" => {
                return Err(unsupported(
                    "`ALTER SEQUENCE ... OWNED BY` is not supported by the cluster SQL gateway, the sequence is independent",
                ));
            }
            "AS" | "START" => {
                return Err(unsupported(format!(
                    "`ALTER SEQUENCE ... {word}` is not supported by the cluster SQL gateway, recreate the sequence instead"
                )));
            }
            other => {
                return Err(unsupported(format!(
                    "`ALTER SEQUENCE ... {other}` is not supported by the cluster SQL gateway"
                )));
            }
        }
        rest = after;
    }
    Ok(PgObjectStatement::AlterSequence { name, alteration })
}

/// Parses `DROP SEQUENCE [IF EXISTS] name[, ...]`.
fn parse_drop_sequence(tail: &str) -> Result<PgObjectStatement, SqlFrontendError> {
    let mut rest = tail.trim_start();
    let mut if_exists = false;
    if let Some(after) = strip_keyword(rest, "IF") {
        rest = strip_keyword(after.trim_start(), "EXISTS").ok_or_else(|| syntax("`DROP SEQUENCE IF` requires `EXISTS`"))?.trim_start();
        if_exists = true;
    }
    let names = split_identifiers(rest)?;
    if names.is_empty() {
        return Err(syntax("`DROP SEQUENCE` needs at least one name"));
    }
    Ok(PgObjectStatement::DropSequence { names, if_exists })
}

/// Parses `GRANT role[, ...] TO member[, ...]` and its `REVOKE` form.
fn parse_membership(rest: &str, granting: bool) -> Result<PgObjectStatement, SqlFrontendError> {
    let rest = rest.trim_start();
    let separator = if granting { "TO" } else { "FROM" };
    if !granting
        && let Some(after) = strip_keyword(rest, "ADMIN")
    {
        let after = strip_keyword(after, "OPTION").ok_or_else(|| syntax("`REVOKE ADMIN` requires `OPTION FOR`"))?;
        strip_keyword(after.trim_start(), "FOR").ok_or_else(|| syntax("`REVOKE ADMIN OPTION` requires `FOR`"))?;
        return Err(unsupported(
            "`REVOKE ADMIN OPTION FOR` is not supported by the cluster SQL gateway, revoke the membership instead",
        ));
    }
    let (roles_text, after_roles) = split_keyword(rest, separator)
        .ok_or_else(|| syntax(format!("`{}` requires `{separator} <role>`", if granting { "GRANT" } else { "REVOKE" })))?;
    let roles = split_identifiers(&roles_text)?;
    if roles.is_empty() {
        return Err(syntax("the statement names no role"));
    }
    let after_roles = after_roles.trim_start();
    if granting && strip_keyword(after_roles, "WITH").is_some() {
        return Err(unsupported("`GRANT ... WITH ADMIN OPTION` is not supported by the cluster SQL gateway"));
    }
    let members = split_identifiers(after_roles)?;
    if members.is_empty() {
        return Err(syntax("the statement names no member role"));
    }
    Ok(if granting {
        PgObjectStatement::GrantMembership { roles, members }
    }
    else {
        PgObjectStatement::RevokeMembership { roles, members }
    })
}

fn parse_privileges(text: &str) -> Result<Vec<PgPrivilege>, SqlFrontendError> {
    let mut privileges = Vec::new();
    for part in text.split(',') {
        let name = part.trim();
        if name.eq_ignore_ascii_case("ALL") || name.eq_ignore_ascii_case("ALL PRIVILEGES") {
            for privilege in [PgPrivilege::Select, PgPrivilege::Insert, PgPrivilege::Update, PgPrivilege::Delete] {
                if !privileges.contains(&privilege) {
                    privileges.push(privilege);
                }
            }
            continue;
        }
        let Some(privilege) = PgPrivilege::parse(name)
        else {
            return Err(unsupported(format!(
                "privilege `{name}` is not supported by the cluster SQL gateway, use SELECT, INSERT, UPDATE or DELETE"
            )));
        };
        if !privileges.contains(&privilege) {
            privileges.push(privilege);
        }
    }
    if privileges.is_empty() {
        return Err(syntax("the statement names no privilege"));
    }
    Ok(privileges)
}

fn split_identifiers(source: &str) -> Result<Vec<String>, SqlFrontendError> {
    let mut names = Vec::new();
    let mut rest = source.trim();
    while !rest.is_empty() {
        let (name, after) = take_identifier(rest)?;
        names.push(name);
        rest = after.trim_start();
        if let Some(after) = rest.strip_prefix(',') {
            rest = after.trim_start();
        }
        else {
            break;
        }
    }
    if !rest.trim().is_empty() {
        return Err(unsupported("trailing tokens after the role list are not supported by the cluster SQL gateway"));
    }
    Ok(names)
}

fn if_not_exists(source: &str) -> (&str, bool) {
    let Some(rest) = strip_keyword(source, "IF") else {
        return (source, false);
    };
    let rest = strip_keyword(rest.trim_start(), "NOT").unwrap_or(rest).trim_start();
    match strip_keyword(rest, "EXISTS") {
        Some(after) => (after.trim_start(), true),
        None => (source, false),
    }
}

fn split_keyword<'a>(source: &'a str, keyword: &str) -> Option<(String, &'a str)> {
    let upper = source.to_ascii_uppercase();
    let needle = format!(" {keyword} ");
    let index = upper.find(&needle)?;
    Some((source[..index].trim().to_string(), &source[index + needle.len()..]))
}

fn take_identifier(source: &str) -> Result<(String, &str), SqlFrontendError> {
    let trimmed = source.trim_start();
    if let Some(inner) = trimmed.strip_prefix('"') {
        let Some(end) = inner.find('"') else {
            return Err(syntax("unterminated quoted name"));
        };
        return Ok((inner[..end].to_owned(), &inner[end + 1..]));
    }
    let end = trimmed.find(|character: char| character.is_whitespace() || character == ',' || character == ';' || character == '.')
        .unwrap_or(trimmed.len());
    if end == 0 {
        return Err(syntax("expected an identifier"));
    }
    Ok((trimmed[..end].to_owned(), &trimmed[end..]))
}

fn first_word(source: &str) -> Option<&str> {
    let trimmed = source.trim_start();
    let end = trimmed.find(|character: char| !character.is_ascii_alphabetic()).unwrap_or(trimmed.len());
    if end == 0 {
        return None;
    }
    Some(&trimmed[..end])
}

fn strip_keyword<'a>(source: &'a str, keyword: &str) -> Option<&'a str> {
    let trimmed = source.trim_start();
    let head = &trimmed[..trimmed.len().min(keyword.len())];
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = &trimmed[keyword.len()..];
    if rest.starts_with(|character: char| character.is_alphanumeric() || character == '_') {
        return None;
    }
    Some(rest)
}

fn syntax(message: impl Into<String>) -> SqlFrontendError {
    SqlFrontendError::new(SqlErrorKind::Syntax, message)
}

fn unsupported(message: impl Into<String>) -> SqlFrontendError {
    SqlFrontendError::new(SqlErrorKind::Unsupported, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_database_and_roles_are_parsed() {
        assert_eq!(
            parse_pg_object_statement("CREATE DATABASE app").unwrap(),
            Some(PgObjectStatement::CreateDatabase { name: "app".to_owned(), if_not_exists: false })
        );
        assert_eq!(
            parse_pg_object_statement("CREATE ROLE reader WITH LOGIN").unwrap(),
            Some(PgObjectStatement::CreateRole {
                role: PgRole { name: "reader".to_owned(), superuser: false, createdb: false, login: true, inherit: true, password: None },
                password: None,
                if_not_exists: false,
            })
        );
        assert_eq!(
            parse_pg_object_statement("CREATE USER app_user SUPERUSER").unwrap(),
            Some(PgObjectStatement::CreateRole {
                role: PgRole { name: "app_user".to_owned(), superuser: true, createdb: false, login: true, inherit: true, password: None },
                password: None,
                if_not_exists: false,
            })
        );
        assert_eq!(
            parse_pg_object_statement("CREATE ROLE app PASSWORD 'it''s' NOLOGIN").unwrap(),
            Some(PgObjectStatement::CreateRole {
                role: PgRole { name: "app".to_owned(), superuser: false, createdb: false, login: false, inherit: true, password: None },
                password: Some("it's".to_owned()),
                if_not_exists: false,
            })
        );
        assert_eq!(
            parse_pg_object_statement("CREATE ROLE blank PASSWORD NULL").unwrap(),
            Some(PgObjectStatement::CreateRole {
                role: PgRole { name: "blank".to_owned(), superuser: false, createdb: false, login: false, inherit: true, password: None },
                password: None,
                if_not_exists: false,
            })
        );
    }

    #[test]
    fn grants_and_revokes_name_privileges_tables_and_roles() {
        assert_eq!(
            parse_pg_object_statement("GRANT SELECT, INSERT ON public.orders TO reader, writer").unwrap_err().kind,
            SqlErrorKind::Unsupported
        );
        assert_eq!(
            parse_pg_object_statement("GRANT SELECT, INSERT ON orders TO reader, writer").unwrap(),
            Some(PgObjectStatement::Grant {
                privileges: vec![PgPrivilege::Select, PgPrivilege::Insert],
                table: "orders".to_owned(),
                roles: vec!["reader".to_owned(), "writer".to_owned()],
            })
        );
        assert_eq!(
            parse_pg_object_statement("REVOKE ALL ON orders FROM reader").unwrap(),
            Some(PgObjectStatement::Revoke {
                privileges: vec![PgPrivilege::Select, PgPrivilege::Insert, PgPrivilege::Update, PgPrivilege::Delete],
                table: "orders".to_owned(),
                roles: vec!["reader".to_owned()],
            })
        );
    }

    #[test]
    fn membership_grants_parse_without_an_on_clause() {
        assert_eq!(
            parse_pg_object_statement("GRANT reader, writer TO app, worker").unwrap(),
            Some(PgObjectStatement::GrantMembership {
                roles: vec!["reader".to_owned(), "writer".to_owned()],
                members: vec!["app".to_owned(), "worker".to_owned()],
            })
        );
        assert_eq!(
            parse_pg_object_statement("REVOKE reader FROM app").unwrap(),
            Some(PgObjectStatement::RevokeMembership { roles: vec!["reader".to_owned()], members: vec!["app".to_owned()] })
        );
        // `WITH ADMIN OPTION` and `REVOKE ADMIN OPTION FOR` stay explicit rejections.
        assert_eq!(
            parse_pg_object_statement("REVOKE ADMIN OPTION FOR reader FROM app").unwrap_err().kind,
            SqlErrorKind::Unsupported
        );
        assert_eq!(parse_pg_object_statement("GRANT reader TO").unwrap_err().kind, SqlErrorKind::Syntax);
    }

    #[test]
    fn bulk_grants_name_a_schema_and_reject_alter_default_privileges() {
        assert_eq!(
            parse_pg_object_statement("GRANT SELECT, INSERT ON ALL TABLES IN SCHEMA public TO reader").unwrap(),
            Some(PgObjectStatement::GrantAllTables {
                schema: "public".to_owned(),
                privileges: vec![PgPrivilege::Select, PgPrivilege::Insert],
                roles: vec!["reader".to_owned()],
            })
        );
        assert_eq!(
            parse_pg_object_statement("REVOKE ALL ON ALL TABLES IN SCHEMA public FROM reader").unwrap(),
            Some(PgObjectStatement::RevokeAllTables {
                schema: "public".to_owned(),
                privileges: vec![PgPrivilege::Select, PgPrivilege::Insert, PgPrivilege::Update, PgPrivilege::Delete],
                roles: vec!["reader".to_owned()],
            })
        );
        for statement in [
            "GRANT SELECT ON ALL SEQUENCES IN SCHEMA public TO reader",
            "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO reader",
        ] {
            assert_eq!(parse_pg_object_statement(statement).unwrap_err().kind, SqlErrorKind::Unsupported, "{statement}");
        }
    }

    #[test]
    fn sequences_parse_their_options_and_reject_owned_by() {
        assert_eq!(
            parse_pg_object_statement("CREATE SEQUENCE IF NOT EXISTS s START WITH 10 INCREMENT BY 5 MAXVALUE 100 CYCLE").unwrap(),
            Some(PgObjectStatement::CreateSequence {
                sequence: PgSequence::new("s", 10, 5, 1, 100, true),
                if_not_exists: true,
            })
        );
        assert_eq!(
            parse_pg_object_statement("CREATE SEQUENCE small AS SMALLINT").unwrap(),
            Some(PgObjectStatement::CreateSequence {
                sequence: PgSequence::new("small", 1, 1, 1, i16::MAX as i64, false),
                if_not_exists: false,
            })
        );
        assert_eq!(
            parse_pg_object_statement("ALTER SEQUENCE s INCREMENT 2 RESTART WITH 7 NO CYCLE").unwrap(),
            Some(PgObjectStatement::AlterSequence {
                name: "s".to_owned(),
                alteration: PgSequenceAlteration {
                    increment: Some(2),
                    restart: Some(Some(7)),
                    min_value: None,
                    max_value: None,
                    cycle: Some(false),
                },
            })
        );
        assert_eq!(
            parse_pg_object_statement("DROP SEQUENCE IF EXISTS a, b").unwrap(),
            Some(PgObjectStatement::DropSequence { names: vec!["a".to_owned(), "b".to_owned()], if_exists: true })
        );
        assert_eq!(parse_pg_object_statement("CREATE SEQUENCE s OWNED BY t.id").unwrap_err().kind, SqlErrorKind::Unsupported);
        // `DROP TABLE` still belongs to the schema registry.
        assert_eq!(parse_pg_object_statement("DROP TABLE t").unwrap(), None);
        assert_eq!(parse_pg_object_statement("ALTER TABLE t ADD COLUMN c INT").unwrap(), None);
    }

    #[test]
    fn unsupported_clauses_are_rejected_with_a_reason() {
        for statement in [
            "CREATE DATABASE app TEMPLATE template0",
            "GRANT ALL TABLES IN SCHEMA public TO app",
            "GRANT reader TO app WITH ADMIN OPTION",
            "GRANT TRUNCATE ON orders TO app",
        ] {
            assert_eq!(parse_pg_object_statement(statement).unwrap_err().kind, SqlErrorKind::Unsupported, "{statement}");
        }
        // A password that is not a literal, or a literal that never closes, is a
        // syntax error instead of an unsupported clause.
        for statement in ["CREATE ROLE app PASSWORD hidden", "CREATE ROLE app PASSWORD 'unterminated"] {
            assert_eq!(parse_pg_object_statement(statement).unwrap_err().kind, SqlErrorKind::Syntax, "{statement}");
        }
        assert_eq!(parse_pg_object_statement("SELECT 1").unwrap(), None);
    }
}
