//! Catalog uniqueness metadata shared by schema catalog and write paths.

use std::{collections::BTreeSet, fmt};

use crate::Value;

/// One `WHERE` condition of a [`PartialPredicate`].
///
/// `column` is a canonical catalog column name of the same table the rule
/// covers; it does not have to be one of the rule columns. The literal forms
/// follow the VOS `[unique(name where ...)]` contract: integers, floats,
/// booleans and quoted text.
#[derive(Debug, Clone, PartialEq)]
pub enum PredicateCondition {
    /// `column = <literal>`: the row participates when its value equals the literal.
    Equal {
        /// Predicate column name.
        column: String,
        /// Comparison literal.
        value: Value,
    },
    /// `column <> <literal>`: the row participates when its non-null value differs.
    NotEqual {
        /// Predicate column name.
        column: String,
        /// Comparison literal.
        value: Value,
    },
    /// `column in (<literal>, ...)`: the row participates when its non-null
    /// value equals one element. The element list is never empty.
    In {
        /// Predicate column name.
        column: String,
        /// Candidate literals.
        values: Vec<Value>,
    },
    /// `column is not null`: the row participates when the value is present.
    IsNotNull {
        /// Predicate column name.
        column: String,
    },
}

impl PredicateCondition {
    /// The column this condition reads.
    pub fn column(&self) -> &str {
        match self {
            Self::Equal { column, .. } | Self::NotEqual { column, .. } | Self::In { column, .. } | Self::IsNotNull { column } => column,
        }
    }
}

impl fmt::Display for PredicateCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Equal { column, value } => write!(f, "{column} = {}", predicate_literal(value)),
            Self::NotEqual { column, value } => write!(f, "{column} <> {}", predicate_literal(value)),
            Self::In { column, values } => {
                let elements: Vec<String> = values.iter().map(predicate_literal).collect();
                write!(f, "{column} in ({})", elements.join(", "))
            }
            Self::IsNotNull { column } => write!(f, "{column} is not null"),
        }
    }
}

/// Renders one predicate literal in its VOS spelling for diagnostics.
fn predicate_literal(value: &Value) -> String {
    match value {
        Value::Text(text) => format!("'{}'", text.replace('\'', "''")),
        Value::I64(number) => number.to_string(),
        Value::F64(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => format!("{other:?}"),
    }
}

/// The participation predicate of a partial unique rule, mirroring
/// PostgreSQL `CREATE UNIQUE INDEX ... WHERE pred`.
///
/// A row joins the rule's conflict set only when every condition holds, and
/// conditions are combined with logical `and`. The condition list is never
/// empty: a rule without a predicate is a whole-table rule.
#[derive(Debug, Clone, PartialEq)]
pub struct PartialPredicate {
    /// Conditions combined with `and`, in declaration order.
    pub conditions: Vec<PredicateCondition>,
}

impl fmt::Display for PartialPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let conditions: Vec<String> = self.conditions.iter().map(|condition| condition.to_string()).collect();
        write!(f, "{}", conditions.join(" and "))
    }
}

/// A uniqueness rule over one or more columns of a single catalog type.
///
/// `columns` holds canonical catalog column names in declaration order.
/// `nulls_distinct` selects the SQL standard NULL rule:
///
/// - `true` (SQL standard): a row with any NULL in the key never conflicts.
/// - `false`: NULL is treated as an ordinary key component, so repeated NULLs
///   collide.
///
/// `predicate` restricts the rows that participate at all (partial unique
/// index); `None` keeps the rule whole-table.
#[derive(Debug, Clone, PartialEq)]
pub struct UniqueConstraint {
    name: String,
    columns: Vec<String>,
    nulls_distinct: bool,
    predicate: Option<PartialPredicate>,
}

impl UniqueConstraint {
    /// Validates and builds a constraint.
    pub fn new(name: impl Into<String>, columns: Vec<String>, nulls_distinct: bool) -> Result<Self, UniqueConstraintError> {
        let name = name.into();
        if name.is_empty() {
            return Err(UniqueConstraintError::EmptyName);
        }
        if columns.is_empty() {
            return Err(UniqueConstraintError::EmptyColumns { name });
        }
        let mut seen = BTreeSet::new();
        for column in &columns {
            if column.is_empty() {
                return Err(UniqueConstraintError::EmptyColumn { name });
            }
            if !seen.insert(column.as_str()) {
                return Err(UniqueConstraintError::DuplicateColumn { name, column: column.clone() });
            }
        }
        Ok(Self { name, columns, nulls_distinct, predicate: None })
    }

    /// Attaches a participation predicate, making the rule a partial unique
    /// index: only rows whose values satisfy every condition join the
    /// conflict set.
    #[must_use]
    pub fn with_predicate(mut self, predicate: PartialPredicate) -> Self {
        self.predicate = Some(predicate);
        self
    }

    /// Constraint name exposed to hosts and diagnostics.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Ordered column set covered by this constraint.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Whether NULL key components keep rows distinct (SQL standard) or collide.
    pub fn nulls_distinct(&self) -> bool {
        self.nulls_distinct
    }

    /// The participation predicate, `None` for a whole-table rule.
    pub fn predicate(&self) -> Option<&PartialPredicate> {
        self.predicate.as_ref()
    }
}

impl fmt::Display for UniqueConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}){}", self.name, self.columns.join(", "), if self.nulls_distinct { " nulls distinct" } else { " nulls equal" })?;
        if let Some(predicate) = &self.predicate {
            write!(f, " where {predicate}")?;
        }
        Ok(())
    }
}

/// A [`UniqueConstraint`] bound to one catalog table.
#[derive(Debug, Clone, PartialEq)]
pub struct TableUniqueConstraint {
    table: String,
    constraint: UniqueConstraint,
}

impl TableUniqueConstraint {
    /// Validates and binds a constraint to a storage table name.
    pub fn new(table: impl Into<String>, constraint: UniqueConstraint) -> Result<Self, UniqueConstraintError> {
        let table = table.into();
        if table.is_empty() {
            return Err(UniqueConstraintError::EmptyTable);
        }
        Ok(Self { table, constraint })
    }

    /// Storage table name.
    pub fn table(&self) -> &str {
        &self.table
    }

    /// Bound constraint.
    pub fn constraint(&self) -> &UniqueConstraint {
        &self.constraint
    }
}

/// Rejection raised when a [`UniqueConstraint`] declaration is malformed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UniqueConstraintError {
    /// Constraint name is empty.
    EmptyName,
    /// Table name is empty.
    EmptyTable,
    /// Constraint covers no columns.
    EmptyColumns {
        /// Declared constraint name.
        name: String,
    },
    /// One column entry is empty.
    EmptyColumn {
        /// Declared constraint name.
        name: String,
    },
    /// The same column appears twice in one constraint.
    DuplicateColumn {
        /// Declared constraint name.
        name: String,
        /// Repeated column name.
        column: String,
    },
}

impl fmt::Display for UniqueConstraintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => f.write_str("unique constraint name must not be empty"),
            Self::EmptyTable => f.write_str("unique constraint table name must not be empty"),
            Self::EmptyColumns { name } => write!(f, "unique constraint `{name}` has no columns"),
            Self::EmptyColumn { name } => write!(f, "unique constraint `{name}` has an empty column"),
            Self::DuplicateColumn { name, column } => {
                write!(f, "unique constraint `{name}` repeats column `{column}`")
            }
        }
    }
}

impl std::error::Error for UniqueConstraintError {}

impl From<UniqueConstraintError> for crate::Error {
    fn from(error: UniqueConstraintError) -> Self {
        Self::Schema { message: error.to_string() }
    }
}
