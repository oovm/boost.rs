//! Uniqueness enforcement for the Phase 1 row store.
//!
//! Constraints come from two places: `@` (`unique`) attributes of the VOS
//! document and rules a host declares explicitly for composite column sets.
//! Both are expressed as [`TableUniqueConstraint`] so the write path has one
//! check. A conflict returns [`database_types::Error::UniqueViolation`] and leaves
//! the record map untouched.

use std::collections::BTreeMap;

use database_formats::yy::row::{encode_tagged, row_key, select_encoded};
use database_storage::local::{load_table, scan_selected};
use database_types::{Error, PartialPredicate, PredicateCondition, Result, TableUniqueConstraint, UniqueConstraint, Value};
use schema_types::{CanonicalScalar, ResolvedContract, ResolvedFieldContract, TypeContractKind};

use super::{catalog, ops::QueryRow};

/// Connection-scoped unique-key index maintained by the VOS write path.
///
/// A uniqueness rule is otherwise checked by scanning every stored row of the
/// table, which makes a bulk import quadratic: the Nth insert re-reads N rows.
/// This index maps `(constraint, key) -> pk text` in memory so a write pays one
/// hash lookup instead of a table scan, and it is the reason a rule-bearing
/// table can still narrow to its exact record keys.
///
/// The index is never persisted, so it never touches the record windows a
/// statement publishes. It is authoritative only while its schema basis matches
/// the active contract, and every path that changes rows without maintaining it
/// — SQL statements, raw record writes, rollbacks — must invalidate it. A cold
/// index rebuilds from the full table the next time a complete write runs.
#[derive(Debug, Default)]
pub struct UniqueIndex {
    tables: std::sync::Mutex<std::collections::HashMap<String, TableIndex>>,
}

#[derive(Debug)]
struct TableIndex {
    basis: (String, u64),
    entries: std::collections::HashMap<(String, String), String>,
}

/// One write's index context: which connection cache to maintain, against
/// which schema basis, and whether the record map can rebuild it.
pub struct WriteIndex<'a> {
    /// The cache to check and maintain.
    pub index: &'a UniqueIndex,
    /// Schema basis the entries describe: document fingerprint and revision.
    pub basis: (String, u64),
    /// Whether the record map the write runs against holds every row of the
    /// table. Rebuilding a cold index from a partial map would silently miss
    /// duplicates, so it is only sound when this is `true`.
    pub complete: bool,
}

impl UniqueIndex {
    /// An empty cache; the first rule-bearing write rebuilds what it needs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every cached table index.
    pub fn invalidate_all(&self) {
        self.tables.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
    }

    /// Whether the cached index for `table` still describes `basis`.
    pub fn is_warm(&self, table: &str, basis: (&str, u64)) -> bool {
        let tables = self.tables.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        tables.get(table).is_some_and(|index| index.basis.0 == basis.0 && index.basis.1 == basis.1)
    }

    /// Check `row` against every rule on `table` and record its keys.
    ///
    /// `old` is the row being replaced: its keys are removed first, so a
    /// rewrite that keeps one value never collides with itself. A cold index
    /// rebuilds from `records` only when `ctx.complete` proves the map holds
    /// every row; otherwise the caller must have taken the full path and this
    /// fails loudly instead of trusting a partial scan.
    pub fn enforce(
        &self,
        ctx: &WriteIndex<'_>,
        records: &BTreeMap<String, Vec<u8>>,
        constraints: &[TableUniqueConstraint],
        table: &str,
        pk_field: &str,
        pk_text: &str,
        old: Option<&QueryRow>,
        row: &QueryRow,
    ) -> Result<()> {
        let applicable: Vec<&UniqueConstraint> =
            constraints.iter().filter(|candidate| candidate.table() == table).map(TableUniqueConstraint::constraint).collect();
        if applicable.is_empty() {
            return Ok(());
        }
        let mut tables = self.tables.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let stale = tables.get(table).is_none_or(|index| index.basis != ctx.basis);
        if stale {
            if !ctx.complete {
                return Err(Error::Unsupported(
                    "the unique-key index is cold and the record map is partial; the caller must take the full path",
                ));
            }
            let mut entries = std::collections::HashMap::new();
            for existing in load_table(records, table)? {
                let Some(pk) = existing.get(pk_field) else {
                    continue;
                };
                let Ok(existing_pk) = value_text(pk) else {
                    continue;
                };
                for constraint in &applicable {
                    if let Some(key) = entry_key(&existing, constraint) {
                        entries.insert(key, existing_pk.clone());
                    }
                }
            }
            tables.insert(table.to_owned(), TableIndex { basis: ctx.basis.clone(), entries });
        }
        let index = tables.get_mut(table).expect("index inserted or found above");
        if let Some(old_row) = old {
            retract_keys(index, &applicable, old_row, pk_text);
        }
        for constraint in &applicable {
            let Some(key) = entry_key(row, constraint) else {
                continue;
            };
            if let Some(owner) = index.entries.get(&key)
                && owner != pk_text
            {
                return Err(Error::UniqueViolation {
                    table: table.to_owned(),
                    constraint: constraint.name().to_owned(),
                    columns: constraint.columns().to_vec(),
                });
            }
        }
        for constraint in &applicable {
            if let Some(key) = entry_key(row, constraint) {
                index.entries.insert(key, pk_text.to_owned());
            }
        }
        Ok(())
    }

    /// Remove a deleted row's keys, keeping the index free of entries that
    /// would block a later reinsertion of the same value.
    pub fn retract(&self, ctx: &WriteIndex<'_>, constraints: &[TableUniqueConstraint], table: &str, pk_text: &str, old: &QueryRow) {
        let applicable: Vec<&UniqueConstraint> =
            constraints.iter().filter(|candidate| candidate.table() == table).map(TableUniqueConstraint::constraint).collect();
        if applicable.is_empty() {
            return;
        }
        let mut tables = self.tables.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(index) = tables.get_mut(table)
            && index.basis == ctx.basis
        {
            retract_keys(index, &applicable, old, pk_text);
        }
    }
}

fn retract_keys(index: &mut TableIndex, applicable: &[&UniqueConstraint], row: &QueryRow, pk_text: &str) {
    for constraint in applicable {
        if let Some(key) = entry_key(row, constraint)
            && index.entries.get(&key).map(String::as_str) == Some(pk_text)
        {
            index.entries.remove(&key);
        }
    }
}

/// The index slot one row occupies under `constraint`, `None` when the row
/// stays outside a partial rule's predicate or its NULL components make it
/// distinct from every other row.
fn entry_key(row: &QueryRow, constraint: &UniqueConstraint) -> Option<(String, String)> {
    if let Some(predicate) = constraint.predicate()
        && !predicate_holds_values(predicate, row)
    {
        return None;
    }
    let (key, has_null) = constraint_key(row, constraint.columns());
    if has_null && constraint.nulls_distinct() {
        return None;
    }
    Some((constraint.name().to_owned(), key))
}

/// Derives unique constraints declared with the VOS `@` / `[unique]` attributes.
///
/// A bare `[unique]` or `@name` yields a single-column rule named after the
/// field. `[unique(group)]` spelled on several fields yields one multi-column
/// rule named after the group, and `[unique(group where ...)]` restricts that
/// rule to the rows whose values satisfy the predicate. The projection matches
/// the execution catalog so the write path and the catalog never disagree
/// about the declared rules.
pub fn from_contract(contract: &ResolvedContract) -> Result<Vec<TableUniqueConstraint>> {
    let mut constraints = Vec::new();
    for entry in contract.types.iter().filter(|entry| entry.kind == TypeContractKind::Table) {
        let table = catalog::table_storage_name(entry)?.to_string();
        let type_name = entry.canonical_path.last().cloned().unwrap_or_else(|| table.clone());
        let mut groups: BTreeMap<String, (Vec<String>, Option<PartialPredicate>)> = BTreeMap::new();
        for field in &entry.fields {
            for attribute in field.attributes.iter().filter(|attribute| attribute.name == "unique") {
                match unique_attribute_rule(&attribute.syntax)? {
                    (Some(group), predicate) => {
                        let (columns, declared) = groups.entry(group.clone()).or_default();
                        if columns.is_empty() {
                            *declared = predicate.clone();
                        } else if *declared != predicate {
                            return Err(Error::Schema {
                                message: format!("unique group `{group}` spells conflicting predicates on table `{table}`"),
                            });
                        }
                        if !columns.contains(&field.canonical_name) {
                            columns.push(field.canonical_name.clone());
                        }
                    }
                    (None, _) => {
                        let constraint = UniqueConstraint::new(
                            format!("{type_name}_{}_key", field.canonical_name),
                            vec![field.canonical_name.clone()],
                            true,
                        )
                        .map_err(Error::from)?;
                        constraints.push(TableUniqueConstraint::new(table.clone(), constraint).map_err(Error::from)?);
                    }
                }
            }
        }
        for (group, (columns, predicate)) in groups {
            if let Some(predicate) = &predicate {
                validate_unique_predicate(&entry.fields, predicate)?;
            }
            let mut constraint = UniqueConstraint::new(group, columns, true).map_err(Error::from)?;
            if let Some(predicate) = predicate {
                constraint = constraint.with_predicate(predicate);
            }
            constraints.push(TableUniqueConstraint::new(table.clone(), constraint).map_err(Error::from)?);
        }
    }
    Ok(constraints)
}

/// Reads the group name and optional participation predicate of a
/// `[unique(...)]` attribute.
///
/// `[unique]` and `@name` carry no argument and stay single-column rules, so
/// they answer `(None, None)`. `[unique(group)]` (the name may be quoted)
/// declares one multi-column rule shared by every field that spells the same
/// group. `[unique(group where <cond> and <cond> ...)]` declares the same
/// rule restricted to the rows whose values satisfy every condition — a
/// partial unique index in the PostgreSQL sense.
///
/// A present-but-malformed argument is an error: silently falling back to a
/// single-column or whole-table rule would change the declared uniqueness
/// without saying so.
pub fn unique_attribute_rule(syntax: &str) -> Result<(Option<String>, Option<PartialPredicate>)> {
    let Some(open) = syntax.find('(')
    else {
        return Ok((None, None));
    };
    let close = syntax
        .rfind(')')
        .ok_or_else(|| Error::Schema { message: format!("malformed unique attribute `{syntax}`: missing `)`") })?;
    if close <= open + 1 {
        return Err(Error::Schema { message: format!("malformed unique attribute `{syntax}`: empty group name") });
    }
    let (name_part, predicate_part) = split_unique_predicate(syntax[open + 1..close].trim());
    let raw = name_part.trim();
    let name = raw.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')).unwrap_or(raw).trim();
    if name.is_empty() || !name.chars().all(|character| character.is_ascii_alphanumeric() || character == '_') {
        return Err(Error::Schema { message: format!("invalid unique group name `{raw}`: use ASCII letters, digits and `_`") });
    }
    let Some(predicate_body) = predicate_part else {
        return Ok((Some(name.to_owned()), None));
    };
    let tokens = lex_unique_attribute_body(predicate_body, syntax)?;
    let mut cursor = UniqueAttributeCursor { tokens, position: 0, syntax: syntax.to_owned() };
    let mut conditions = Vec::new();
    loop {
        conditions.push(cursor.condition()?);
        if cursor.position == cursor.tokens.len() {
            break;
        }
        cursor.expect_keyword("and")?;
    }
    Ok((Some(name.to_owned()), Some(PartialPredicate { conditions })))
}

/// Splits the attribute body at the first standalone `where` keyword.
///
/// The name region keeps its old textual handling so a malformed name still
/// reports `invalid unique group name` instead of a lexing failure; only the
/// predicate region goes through the lexer.
fn split_unique_predicate(body: &str) -> (&str, Option<&str>) {
    let characters: Vec<char> = body.chars().collect();
    const KEYWORD: [char; 5] = ['w', 'h', 'e', 'r', 'e'];
    let mut start = 0;
    while start + KEYWORD.len() <= characters.len() {
        if characters[start..start + KEYWORD.len()] == KEYWORD[..] {
            let before_ok = start == 0 || characters[start - 1].is_whitespace();
            let after_ok = start + KEYWORD.len() == characters.len() || characters[start + KEYWORD.len()].is_whitespace();
            if before_ok && after_ok {
                let byte_start: usize = characters[..start].iter().map(|character| character.len_utf8()).sum();
                return (&body[..byte_start], Some(&body[byte_start + KEYWORD.len()..]));
            }
        }
        start += 1;
    }
    (body, None)
}

/// One token of a `[unique(...)]` argument.
#[derive(Debug, Clone, PartialEq)]
enum UniqueAttributeToken {
    /// Bare identifier or keyword.
    Word(String),
    /// Quoted string literal with the escapes resolved.
    Text(String),
    /// Numeric literal kept in its raw spelling until classification.
    Number(String),
    /// Single-character punctuation.
    Punct(char),
}

fn lex_unique_attribute_body(body: &str, syntax: &str) -> Result<Vec<UniqueAttributeToken>> {
    let malformed = |detail: String| Error::Schema { message: format!("malformed unique attribute `{syntax}`: {detail}") };
    let characters: Vec<char> = body.chars().collect();
    let mut index = 0;
    let mut tokens = Vec::new();
    while index < characters.len() {
        let character = characters[index];
        if character.is_whitespace() {
            index += 1;
            continue;
        }
        if character == '\'' || character == '"' {
            let quote = character;
            index += 1;
            let mut text = String::new();
            let mut closed = false;
            while index < characters.len() {
                let current = characters[index];
                index += 1;
                if current == quote {
                    closed = true;
                    break;
                }
                if current == '\\' {
                    let escape = characters.get(index).copied().ok_or_else(|| malformed("string literal ends with a dangling `\\`".to_owned()))?;
                    if escape != '\'' && escape != '"' && escape != '\\' {
                        return Err(malformed(format!("string literal escape `\\{escape}` is not one of `\\'`, `\\\"`, `\\\\`")));
                    }
                    text.push(escape);
                    index += 1;
                    continue;
                }
                text.push(current);
            }
            if !closed {
                return Err(malformed("string literal is missing its closing quote".to_owned()));
            }
            tokens.push(UniqueAttributeToken::Text(text));
            continue;
        }
        if character.is_ascii_digit() || (character == '-' && characters.get(index + 1).is_some_and(|next| next.is_ascii_digit())) {
            let start = index;
            index += 1;
            let mut seen_dot = false;
            while index < characters.len() {
                let current = characters[index];
                if current.is_ascii_digit() {
                    index += 1;
                } else if current == '.' && !seen_dot && characters.get(index + 1).is_some_and(|next| next.is_ascii_digit()) {
                    seen_dot = true;
                    index += 1;
                } else {
                    break;
                }
            }
            tokens.push(UniqueAttributeToken::Number(characters[start..index].iter().collect()));
            continue;
        }
        if character.is_ascii_alphabetic() || character == '_' {
            let start = index;
            while index < characters.len() && (characters[index].is_ascii_alphanumeric() || characters[index] == '_') {
                index += 1;
            }
            tokens.push(UniqueAttributeToken::Word(characters[start..index].iter().collect()));
            continue;
        }
        if matches!(character, '=' | '<' | '>' | '(' | ')' | ',') {
            tokens.push(UniqueAttributeToken::Punct(character));
            index += 1;
            continue;
        }
        return Err(malformed(format!("unexpected character `{character}`")));
    }
    Ok(tokens)
}

struct UniqueAttributeCursor {
    tokens: Vec<UniqueAttributeToken>,
    position: usize,
    syntax: String,
}

impl UniqueAttributeCursor {
    fn malformed(&self, detail: String) -> Error {
        Error::Schema { message: format!("malformed unique attribute `{}`: {detail}", self.syntax) }
    }

    fn peek(&self) -> Option<&UniqueAttributeToken> {
        self.tokens.get(self.position)
    }

    fn word(&mut self, expected: &str) -> bool {
        if let Some(UniqueAttributeToken::Word(word)) = self.peek()
            && word == expected
        {
            self.position += 1;
            return true;
        }
        false
    }

    fn expect_keyword(&mut self, expected: &str) -> Result<()> {
        if self.word(expected) {
            return Ok(());
        }
        Err(self.malformed(format!("expected `{expected}`")))
    }

    fn literal(&mut self) -> Result<Value> {
        let token = self.peek().cloned().ok_or_else(|| self.malformed("expected a literal".to_owned()))?;
        self.position += 1;
        match token {
            UniqueAttributeToken::Number(text) => {
                if text.contains('.') {
                    text.parse::<f64>()
                        .map(Value::F64)
                        .map_err(|_| self.malformed(format!("numeric literal `{text}` is not a valid 64-bit float")))
                } else {
                    text.parse::<i64>()
                        .map(Value::I64)
                        .map_err(|_| self.malformed(format!("numeric literal `{text}` is not a valid 64-bit integer")))
                }
            }
            UniqueAttributeToken::Text(text) => Ok(Value::Text(text)),
            UniqueAttributeToken::Word(word) if word == "true" => Ok(Value::Bool(true)),
            UniqueAttributeToken::Word(word) if word == "false" => Ok(Value::Bool(false)),
            _ => Err(self.malformed("expected a literal".to_owned())),
        }
    }

    fn condition(&mut self) -> Result<PredicateCondition> {
        let Some(UniqueAttributeToken::Word(column)) = self.peek().cloned() else {
            return Err(self.malformed("expected a column name".to_owned()));
        };
        self.position += 1;
        match self.peek().cloned() {
            Some(UniqueAttributeToken::Punct('=')) => {
                self.position += 1;
                Ok(PredicateCondition::Equal { column, value: self.literal()? })
            }
            Some(UniqueAttributeToken::Punct('<')) => {
                self.position += 1;
                if self.peek() != Some(&UniqueAttributeToken::Punct('>')) {
                    return Err(self.malformed("expected `>` after `<`".to_owned()));
                }
                self.position += 1;
                Ok(PredicateCondition::NotEqual { column, value: self.literal()? })
            }
            Some(UniqueAttributeToken::Word(word)) if word == "in" => {
                self.position += 1;
                if self.peek() != Some(&UniqueAttributeToken::Punct('(')) {
                    return Err(self.malformed("expected `(` after `in`".to_owned()));
                }
                self.position += 1;
                let mut values = vec![self.literal()?];
                while self.peek() == Some(&UniqueAttributeToken::Punct(',')) {
                    self.position += 1;
                    values.push(self.literal()?);
                }
                if self.peek() != Some(&UniqueAttributeToken::Punct(')')) {
                    return Err(self.malformed("expected `,` or `)` inside the `in` list".to_owned()));
                }
                self.position += 1;
                Ok(PredicateCondition::In { column, values })
            }
            Some(UniqueAttributeToken::Word(word)) if word == "is" => {
                self.position += 1;
                self.expect_keyword("not")?;
                self.expect_keyword("null")?;
                Ok(PredicateCondition::IsNotNull { column })
            }
            _ => Err(self.malformed("expected `=`, `<>`, `in` or `is not null`".to_owned())),
        }
    }
}

/// Checks every predicate column against the table's declared fields.
///
/// A predicate may name any column of the same table, including one outside
/// the rule columns. A comparison literal must match the column's declared
/// type family (integers against `I64`, floats against `F64`, `bool` against
/// `Bool`, text against `Text`); a reference column only supports
/// `is not null`.
pub fn validate_unique_predicate(fields: &[ResolvedFieldContract], predicate: &PartialPredicate) -> Result<()> {
    for condition in &predicate.conditions {
        let column = condition.column();
        let field = fields
            .iter()
            .find(|candidate| candidate.canonical_name == column)
            .ok_or_else(|| Error::Schema { message: format!("unique predicate references unknown column `{column}`") })?;
        let literals: &[Value] = match condition {
            PredicateCondition::Equal { value, .. } | PredicateCondition::NotEqual { value, .. } => std::slice::from_ref(value),
            PredicateCondition::In { values, .. } => values,
            PredicateCondition::IsNotNull { .. } => &[],
        };
        if literals.is_empty() {
            continue;
        }
        let mut declared = &field.canonical_type;
        while let schema_types::ResolvedCanonicalType::Optional(inner) = declared {
            declared = inner;
        }
        if matches!(declared, schema_types::ResolvedCanonicalType::Reference(_)) {
            return Err(Error::Schema { message: format!("unique predicate column `{column}` is a reference: only `is not null` is supported") });
        }
        for value in literals {
            let matches = match (declared, value) {
                (schema_types::ResolvedCanonicalType::Scalar(scalar), Value::I64(_)) => {
                    matches!(scalar, CanonicalScalar::I8 | CanonicalScalar::I16 | CanonicalScalar::I32 | CanonicalScalar::I64 | CanonicalScalar::U8 | CanonicalScalar::U16 | CanonicalScalar::U32 | CanonicalScalar::U64)
                }
                (schema_types::ResolvedCanonicalType::Scalar(scalar), Value::F64(_)) => matches!(scalar, CanonicalScalar::F32 | CanonicalScalar::F64),
                (schema_types::ResolvedCanonicalType::Scalar(CanonicalScalar::Bool), Value::Bool(_)) => true,
                (schema_types::ResolvedCanonicalType::Scalar(scalar), Value::Text(_)) => matches!(scalar, CanonicalScalar::Utf8 | CanonicalScalar::Utf16),
                _ => false,
            };
            if !matches {
                return Err(Error::Schema { message: format!("unique predicate literal for column `{column}` does not match the declared type") });
            }
        }
    }
    Ok(())
}

/// Combines document-derived rules with host declarations, deduplicated by
/// `(table, name)` with document rules taking precedence.
pub fn merge(document: Vec<TableUniqueConstraint>, declared: &[TableUniqueConstraint]) -> Vec<TableUniqueConstraint> {
    let mut merged = document;
    for candidate in declared {
        let duplicate = merged
            .iter()
            .any(|existing| existing.table() == candidate.table() && existing.constraint().name() == candidate.constraint().name());
        if !duplicate {
            merged.push(candidate.clone());
        }
    }
    merged
}

/// Whether `table` carries any uniqueness rule at all.
pub fn has_rules(constraints: &[TableUniqueConstraint], table: &str) -> bool {
    constraints.iter().any(|candidate| candidate.table() == table)
}

/// Rejects `row` when it would duplicate an existing key of any constraint on `table`.
///
/// `row` replaces the row with `pk_text`, so that row never conflicts with itself.
/// NULL components follow the constraint's `nulls_distinct` flag and missing
/// fields count as NULL.
///
/// The comparison runs on the tagged bytes of the constraint columns instead of
/// decoded rows: a bulk load inserts thousands of rows into the same table, and
/// decoding each stored row into a map would make every insert cost a full table
/// decode. Byte equality of one column is exactly value equality, so the rule
/// does not get weaker.
///
/// A partial rule adds its predicate columns to the same byte selection: the
/// incoming row is tested once and every stored row is tested per scan, so a
/// row outside the predicate never joins the conflict comparison.
pub fn check(
    records: &BTreeMap<String, Vec<u8>>,
    constraints: &[TableUniqueConstraint],
    table: &str,
    _pk_field: &str,
    pk_text: &str,
    row: &QueryRow,
) -> Result<()> {
    let applicable: Vec<&UniqueConstraint> =
        constraints.iter().filter(|candidate| candidate.table() == table).map(TableUniqueConstraint::constraint).collect();
    if applicable.is_empty() {
        return Ok(());
    }
    let target_key = row_key(table, pk_text);

    for constraint in applicable {
        let mut names: Vec<&str> = constraint.columns().iter().map(String::as_str).collect();
        let rule_width = names.len();
        let mut prepared: Option<PreparedPredicate> = None;
        if let Some(predicate) = constraint.predicate() {
            let mut slots = Vec::with_capacity(predicate.conditions.len());
            for condition in &predicate.conditions {
                let column = condition.column();
                slots.push(match names.iter().position(|name| *name == column) {
                    Some(index) => index,
                    None => {
                        names.push(column);
                        names.len() - 1
                    }
                });
            }
            prepared = Some(PreparedPredicate::build(predicate, &slots)?);
        }
        let incoming = select_encoded(row, &names)?;
        let incoming: Vec<Option<&[u8]>> = incoming.iter().map(|slot| slot.as_deref()).collect();
        if let Some(predicate) = &prepared
            && !predicate.holds(&incoming)
        {
            continue;
        }
        if incoming[..rule_width].iter().any(Option::is_none) && constraint.nulls_distinct() {
            continue;
        }
        let mut violation = false;
        scan_selected(records, table, &names, |key, stored| {
            if key == target_key {
                return Ok(());
            }
            if let Some(predicate) = &prepared
                && !predicate.holds(stored)
            {
                return Ok(());
            }
            if stored.iter().any(Option::is_none) && constraint.nulls_distinct() {
                return Ok(());
            }
            if stored[..rule_width].iter().zip(&incoming[..rule_width]).all(|(stored, incoming)| *stored == *incoming) {
                violation = true;
            }
            Ok(())
        })?;
        if violation {
            return Err(Error::UniqueViolation {
                table: table.to_owned(),
                constraint: constraint.name().to_owned(),
                columns: constraint.columns().to_vec(),
            });
        }
    }
    Ok(())
}

/// One predicate literal already encoded to the row codec's tagged bytes so a
/// stored column slice answers the condition with the same byte equality the
/// rule columns use.
enum PreparedTest {
    Equal(Vec<u8>),
    NotEqual(Vec<u8>),
    In(Vec<Vec<u8>>),
    IsNotNull,
}

struct PreparedCondition {
    slot: usize,
    test: PreparedTest,
}

/// A [`PartialPredicate`] bound to one combined column selection.
struct PreparedPredicate {
    conditions: Vec<PreparedCondition>,
}

impl PreparedPredicate {
    /// Binds every condition to its position in the selection: `slots[i]`
    /// names where condition `i`'s column sits.
    fn build(predicate: &PartialPredicate, slots: &[usize]) -> Result<Self> {
        let mut conditions = Vec::with_capacity(predicate.conditions.len());
        for (condition, slot) in predicate.conditions.iter().zip(slots) {
            let test = match condition {
                PredicateCondition::Equal { value, .. } => PreparedTest::Equal(encode_tagged(value)?),
                PredicateCondition::NotEqual { value, .. } => PreparedTest::NotEqual(encode_tagged(value)?),
                PredicateCondition::In { values, .. } => PreparedTest::In(values.iter().map(encode_tagged).collect::<Result<Vec<_>>>()?),
                PredicateCondition::IsNotNull { .. } => PreparedTest::IsNotNull,
            };
            conditions.push(PreparedCondition { slot: *slot, test });
        }
        Ok(Self { conditions })
    }

    /// Whether one row's selected bytes satisfy every condition. A missing
    /// (NULL) predicate column answers false for the comparisons and true for
    /// `is not null`, matching PostgreSQL.
    fn holds(&self, selected: &[Option<&[u8]>]) -> bool {
        self.conditions.iter().all(|condition| match &condition.test {
            PreparedTest::Equal(bytes) => selected[condition.slot].is_some_and(|value| value == bytes.as_slice()),
            PreparedTest::NotEqual(bytes) => selected[condition.slot].is_some_and(|value| value != bytes.as_slice()),
            PreparedTest::In(list) => selected[condition.slot].is_some_and(|value| list.iter().any(|element| element.as_slice() == value)),
            PreparedTest::IsNotNull => selected[condition.slot].is_some(),
        })
    }
}

/// Whether one in-memory row satisfies every condition directly on values.
///
/// This is the [`PartialPredicate`] twin of [`PreparedPredicate::holds`] for
/// the paths that already hold decoded rows, and it follows the same NULL
/// rule: a missing or NULL column answers false for comparisons, true for
/// `is not null`.
fn predicate_holds_values(predicate: &PartialPredicate, row: &QueryRow) -> bool {
    predicate.conditions.iter().all(|condition| match condition {
        PredicateCondition::Equal { column, value } => matches!(row.get(column), Some(found) if found == value),
        PredicateCondition::NotEqual { column, value } => {
            matches!(row.get(column), Some(found) if !matches!(found, Value::Null) && found != value)
        }
        PredicateCondition::In { column, values } => {
            matches!(row.get(column), Some(found) if !matches!(found, Value::Null) && values.iter().any(|element| element == found))
        }
        PredicateCondition::IsNotNull { column } => matches!(row.get(column), Some(found) if !matches!(found, Value::Null)),
    })
}

/// Same rule as [`check`], evaluated against rows the caller already loaded.
///
/// Multi-row `INSERT` uses this to test a staged row against the stored rows
/// once and against the earlier rows of the same statement without reloading.
pub fn check_against_rows(
    existing_rows: &[QueryRow],
    constraints: &[TableUniqueConstraint],
    table: &str,
    pk_field: &str,
    pk_text: &str,
    row: &QueryRow,
) -> Result<()> {
    let applicable: Vec<&UniqueConstraint> =
        constraints.iter().filter(|candidate| candidate.table() == table).map(TableUniqueConstraint::constraint).collect();
    if applicable.is_empty() {
        return Ok(());
    }

    for constraint in applicable {
        if let Some(predicate) = constraint.predicate()
            && !predicate_holds_values(predicate, row)
        {
            continue;
        }
        let (incoming, incoming_null) = constraint_key(row, constraint.columns());
        if incoming_null && constraint.nulls_distinct() {
            continue;
        }
        for existing in existing_rows {
            let existing_pk = existing.get(pk_field).map(value_text).transpose()?.unwrap_or_default();
            if existing_pk == pk_text {
                continue;
            }
            if let Some(predicate) = constraint.predicate()
                && !predicate_holds_values(predicate, existing)
            {
                continue;
            }
            let (candidate, candidate_null) = constraint_key(existing, constraint.columns());
            if candidate_null && constraint.nulls_distinct() {
                continue;
            }
            if incoming == candidate {
                return Err(Error::UniqueViolation {
                    table: table.to_owned(),
                    constraint: constraint.name().to_owned(),
                    columns: constraint.columns().to_vec(),
                });
            }
        }
    }
    Ok(())
}

/// Whether `incoming` would collide with `existing` under `constraint`.
///
/// Both key halves are compared, and a key that contains `NULL` never collides
/// when the constraint declares NULLs as distinct — the same rule [`check`]
/// applies, exposed so an `ON CONFLICT` clause can detect the row it targets.
pub fn keys_conflict(constraint: &UniqueConstraint, existing: &QueryRow, incoming: &QueryRow) -> bool {
    let (candidate, candidate_null) = constraint_key(existing, constraint.columns());
    if candidate_null && constraint.nulls_distinct() {
        return false;
    }
    let (key, incoming_null) = constraint_key(incoming, constraint.columns());
    if incoming_null && constraint.nulls_distinct() {
        return false;
    }
    candidate == key
}

fn constraint_key(row: &QueryRow, columns: &[String]) -> (String, bool) {
    let mut parts = Vec::with_capacity(columns.len());
    let mut has_null = false;
    for column in columns {
        match row.get(column) {
            None | Some(Value::Null) => {
                has_null = true;
                parts.push(Value::Null.key_component());
            }
            Some(value) => parts.push(value.key_component()),
        }
    }
    (parts.join("\u{1f}"), has_null)
}

/// Renders a value as its canonical text form for key comparison.
pub fn value_text(value: &Value) -> Result<String> {
    Ok(match value {
        Value::Text(text) => text.clone(),
        Value::Uuid(id) => id.to_string(),
        Value::I64(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Date(date) => date.to_iso(),
        Value::Time(time) => time.to_iso(),
        Value::Timestamp(timestamp) => timestamp.to_iso(),
        Value::Decimal(decimal) => decimal.to_plain_string(),
        Value::Null => return Err(Error::Schema { message: "primary key cannot be null".into() }),
        _ => return Err(Error::Schema { message: "primary key must be a scalar value".into() }),
    })
}
