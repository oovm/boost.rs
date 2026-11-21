//! Language-neutral lowering of a resolved schema contract into the local
//! execution catalog.
//!
//! This module does **not** parse or validate any source dialect. It consumes
//! the already-resolved contract produced by the VOS boundary
//! ([`crate::engine::schema_vos`]) or published directly by a host through
//! [`crate::engine::Connection::install_catalog`], and projects it into the
//! typed execution handles (field ids, virtual slots, [`Type`]) the executor
//! binds against. Uniqueness rules derived from contract attributes plus
//! host-declared constraints are part of the same projection.

use std::collections::BTreeMap;

use crate::program::{FieldHandle, LayoutField, RecordLayout, RecordLayoutError, Type};
use database_formats::yy::row::{NamedFieldLayout, NamedValueLayout, NamedVariantLayout, RowSchemaDirectory, ValueLayout};
use database_types::{Error, Result, UniqueConstraint};

/// A type-qualified field path used by [`RenameMap`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FieldPath {
    /// Name of the owning type declaration.
    pub type_name: String,
    /// Current field name.
    pub field_name: String,
}

/// Explicit renames applied when a schema document evolves.
///
/// Type renames map an old type name to its new name; field renames map an old
/// [`FieldPath`] to the new field name. Unlisted types and fields keep their
/// durable identity while their spelling is unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenameMap {
    /// Old type name to new type name.
    pub types: BTreeMap<String, String>,
    /// Old field path to new field name.
    pub fields: BTreeMap<FieldPath, String>,
}

impl RenameMap {
    pub(crate) fn type_target(&self, name: &str) -> Option<&str> {
        self.types.get(name).map(String::as_str)
    }

    pub(crate) fn field_target(&self, type_name: &str, field_name: &str) -> Option<&str> {
        self.fields.get(&FieldPath { type_name: type_name.to_owned(), field_name: field_name.to_owned() }).map(String::as_str)
    }
}

/// Initial execution-facing catalog built from one validated VOS document.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionCatalog {
    /// Cataloged table and class types in VOS document order.
    pub types: Vec<ExecutionType>,
}

/// One VOS table, class or numeric type projected into the execution model.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionType {
    /// Stable VOS TypeId used as the execution schema identity.
    pub schema_id: u64,
    /// Current VOS type name.
    pub name: String,
    /// Catalog kind.
    pub kind: ExecutionTypeKind,
    /// Fields in stable virtual-slot order.
    pub fields: Vec<ExecutionField>,
    /// Declared members of an `enums` or `flags` type. Empty otherwise.
    pub members: Vec<ExecutionMember>,
    /// Declared variants of a `unite` type. Empty otherwise.
    pub variants: Vec<ExecutionVariant>,
    /// True when the declared layout is closed (`structure`, `unite`).
    pub closed: bool,
    /// Uniqueness rules derived from the VOS document plus host declarations.
    pub unique_constraints: Vec<UniqueConstraint>,
}

/// One declared `unite` variant, kept as catalog metadata.
///
/// The discriminant is the variant's durable physical identity: the row codec
/// stores it with the payload, so a rename keeps old records readable while a
/// reordered declaration changes the layout revision and rejects them instead of
/// binding the bytes to another variant.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionVariant {
    /// Declared variant name.
    pub name: String,
    /// Stable discriminant (source-order index).
    pub discriminant: i64,
    /// Closed payload fields.
    pub fields: Vec<ExecutionVariantField>,
}

/// One payload field of a `unite` variant.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionVariantField {
    /// Declared field name.
    pub name: String,
    /// Lowered execution type.
    pub ty: Type,
}

/// One declared member of an `enums` or `flags` type, in source order.
///
/// `value` is the declared number: an `enums` member's stable identity, or one
/// bit of a `flags` mask. Both kinds are stored as `i64`, so the catalog keeps
/// this table to recover the named semantics when a value is bound or decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionMember {
    /// Declared member name.
    pub name: String,
    /// Declared number for `enums`, single-bit mask for `flags`.
    pub value: i64,
}

/// Type declaration kind used by the local execution catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionTypeKind {
    /// Persistent table declaration.
    Table,
    /// Inline value/class declaration.
    Class,
    /// Named numeric type whose values are declared members.
    Enums,
    /// Named bit-mask type whose members are declared bits.
    Flags,
    /// Tagged union type whose variants are declared values.
    Unite,
    /// Closed field record type.
    Structure,
}

/// One VOS field projected into a typed execution handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionField {
    /// Stable VOS FieldId.
    pub field_id: u64,
    /// Durable virtual slot used by the physical row layout.
    pub virtual_field: u32,
    /// Current VOS field name.
    pub name: String,
    /// Schema-bound execution handle.
    pub handle: FieldHandle,
}

impl ExecutionType {
    /// Converts catalog fields into the published execution layout contract.
    pub fn layout(&self) -> std::result::Result<RecordLayout, RecordLayoutError> {
        RecordLayout::new(
            self.schema_id,
            self.fields
                .iter()
                .map(|field| LayoutField { field_id: field.field_id, index: field.virtual_field, ty: field.handle.ty() })
                .collect(),
        )
    }
}

/// Lower a strict resolved contract into the local execution catalog.
///
/// This is the downstream path for new integrations. It does not parse source,
/// inspect a syntax AST, or infer identities from declaration order.
pub fn execution_catalog_from_resolved_contract(contract: &schema_types::ResolvedContract) -> Result<ExecutionCatalog> {
    contract
        .validate()
        .map_err(|error| Error::Schema { message: format!("invalid resolved VOS contract {}: {}", error.code, error.message) })?;
    database_query::local::validate_contract_types(contract)?;
    let mut types = Vec::with_capacity(contract.types.len());
    for item in &contract.types {
        let kind = match item.kind {
            schema_types::TypeContractKind::Table => ExecutionTypeKind::Table,
            schema_types::TypeContractKind::Class => ExecutionTypeKind::Class,
            schema_types::TypeContractKind::Enums => ExecutionTypeKind::Enums,
            schema_types::TypeContractKind::Flags => ExecutionTypeKind::Flags,
            schema_types::TypeContractKind::Unite => ExecutionTypeKind::Unite,
            schema_types::TypeContractKind::Structure => ExecutionTypeKind::Structure,
        };
        let name = item
            .canonical_path
            .last()
            .cloned()
            .ok_or_else(|| Error::Schema { message: "resolved VOS type has an empty canonical path".into() })?;
        let mut fields = Vec::with_capacity(item.fields.len());
        let mut unique_constraints = Vec::new();
        // `[unique(group)]` on several fields is one multi-column rule; a bare
        // `[unique]` / `@name` stays a single-column rule derived from the field
        // name.
        let mut unique_groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for field in &item.fields {
            let virtual_field_index = field.virtual_field_index.ok_or_else(|| Error::Schema {
                message: format!("resolved VOS field `{}` has no durable virtual slot", field.canonical_name),
            })?;
            let ty = resolved_execution_type(&field.canonical_type, contract)?;
            let handle = FieldHandle::new(item.type_id, field.field_id, virtual_field_index, ty)
                .map_err(|_| Error::Schema { message: "resolved VOS contract emitted an invalid field identity".into() })?;
            if kind == ExecutionTypeKind::Table {
                for attribute in field.attributes.iter().filter(|attribute| attribute.name == "unique") {
                    match database_query::local::constraints::unique_attribute_rule(&attribute.syntax)? {
                        Some(group) => {
                            let columns = unique_groups.entry(group).or_default();
                            if !columns.contains(&field.canonical_name) {
                                columns.push(field.canonical_name.clone());
                            }
                        }
                        None => unique_constraints.push(
                            UniqueConstraint::new(format!("{name}_{}_key", field.canonical_name), vec![field.canonical_name.clone()], true)
                                .map_err(|error| Error::Schema { message: format!("invalid unique constraint from VOS attribute: {error}") })?,
                        ),
                    }
                }
            }
            fields.push(ExecutionField {
                field_id: field.field_id,
                virtual_field: virtual_field_index,
                name: field.canonical_name.clone(),
                handle,
            });
        }
        for (group, columns) in unique_groups {
            let constraint = UniqueConstraint::new(group, columns, true)
                .map_err(|error| Error::Schema { message: format!("invalid grouped unique constraint from VOS attribute: {error}") })?;
            unique_constraints.push(constraint);
        }
        let mut variants = Vec::with_capacity(item.variants.len());
        for variant in &item.variants {
            let mut payload = Vec::with_capacity(variant.fields.len());
            for field in &variant.fields {
                payload.push(ExecutionVariantField {
                    name: field.canonical_name.clone(),
                    ty: resolved_execution_type(&field.canonical_type, contract)?,
                });
            }
            variants.push(ExecutionVariant { name: variant.canonical_name.clone(), discriminant: variant.discriminant, fields: payload });
        }
        types.push(ExecutionType {
            schema_id: item.type_id,
            name,
            kind,
            fields,
            members: item.members.iter().map(|member| ExecutionMember { name: member.canonical_name.clone(), value: member.value }).collect(),
            variants,
            closed: item.closed,
            unique_constraints,
        });
    }
    Ok(ExecutionCatalog { types })
}

impl ExecutionCatalog {
    /// Adds persisted host-declared uniqueness rules, validating every table
    /// and column against the catalog.
    ///
    /// Document-derived rules win when a declared rule reuses their name.
    pub fn apply_declared_unique_constraints(&mut self, declared: &[database_types::TableUniqueConstraint]) -> Result<()> {
        for declared in declared {
            let entry =
                self.types.iter_mut().find(|entry| entry.kind == ExecutionTypeKind::Table && entry.name == declared.table()).ok_or_else(
                    || Error::Schema {
                        message: format!("unique constraint `{}` refers to unknown table `{}`", declared.constraint().name(), declared.table()),
                    },
                )?;
            for column in declared.constraint().columns() {
                if !entry.fields.iter().any(|field| &field.name == column) {
                    return Err(Error::Schema {
                        message: format!(
                            "unique constraint `{}` refers to unknown column `{column}` on table `{}`",
                            declared.constraint().name(),
                            declared.table()
                        ),
                    });
                }
            }
            if entry.unique_constraints.iter().any(|existing| existing.name() == declared.constraint().name()) {
                continue;
            }
            entry.unique_constraints.push(declared.constraint().clone());
        }
        Ok(())
    }

    /// Whether `table` already owns a constraint with this name.
    pub fn has_unique_constraint(&self, table: &str, name: &str) -> bool {
        self.types
            .iter()
            .filter(|entry| entry.name == table)
            .any(|entry| entry.unique_constraints.iter().any(|constraint| constraint.name() == name))
    }
}

/// Applies a migration rename map to persisted unique-constraint metadata.
///
/// Constraint names stay stable, while table and column names follow the new
/// schema so the rule keeps pointing at the renamed identity.
pub fn remap_unique_constraints(
    constraints: &[database_types::TableUniqueConstraint],
    renames: &RenameMap,
) -> Result<Vec<database_types::TableUniqueConstraint>> {
    constraints
        .iter()
        .map(|declared| {
            let table = renames.types.get(declared.table()).map(String::as_str).unwrap_or(declared.table());
            let rename_field = |field: &str| {
                renames
                    .fields
                    .get(&FieldPath { type_name: declared.table().to_owned(), field_name: field.to_owned() })
                    .cloned()
                    .unwrap_or_else(|| field.to_owned())
            };
            let columns = declared.constraint().columns().iter().map(|column| rename_field(column)).collect::<Vec<_>>();
            let constraint =
                UniqueConstraint::new(declared.constraint().name(), columns, declared.constraint().nulls_distinct()).map_err(Error::from)?;
            database_types::TableUniqueConstraint::new(table, constraint).map_err(Error::from)
        })
        .collect()
}

/// Lowers one resolved VOS canonical type into an execution type.
///
/// A `file` keeps its descriptor-and-manifest value shape, and a `vector<N>`
/// becomes the execution vector of the declared dimension. VOS fixes the
/// element precision and the dimension and leaves distance metrics to query
/// capabilities, so no metric is bound here.
///
/// `T?`, `&T` and `[T]` are physical wrappers rather than new storage kinds:
///
/// - `T?` is transparent because a nullable column is a `null` value in the row,
/// - `&T` stores the referenced row's primary key, so it lowers as that key,
/// - `[T]` lowers to a list whose elements are checked at write time.
fn resolved_execution_type(ty: &schema_types::ResolvedCanonicalType, contract: &schema_types::ResolvedContract) -> Result<Type> {
    resolved_execution_type_at(ty, contract, 0)
}

fn resolved_execution_type_at(
    ty: &schema_types::ResolvedCanonicalType,
    contract: &schema_types::ResolvedContract,
    depth: u8,
) -> Result<Type> {
    use schema_types::ResolvedCanonicalType;
    if depth > MAX_TYPE_DEPTH {
        return Err(Error::Schema { message: "resolved VOS type nests deeper than the supported depth".into() });
    }
    match ty {
        ResolvedCanonicalType::Scalar(scalar) => execution_type_for_scalar(*scalar),
        ResolvedCanonicalType::Generic { path, arguments } if path == &["Decimal"] => decimal_type_from_arguments(arguments),
        ResolvedCanonicalType::File => Ok(Type::File),
        ResolvedCanonicalType::Vector { dimension, .. } => Ok(Type::Vector { dimension: *dimension }),
        // A nullable column stores the same execution type; absence is a null value.
        ResolvedCanonicalType::Optional(inner) => resolved_execution_type_at(inner, contract, depth + 1),
        // A reference stores the target row's primary key, not the row.
        ResolvedCanonicalType::Reference(inner) => match inner.as_ref() {
            ResolvedCanonicalType::User { type_id, .. } => {
                let key = reference_key_type(contract, *type_id)?;
                resolved_execution_type_at(key, contract, depth + 1)
            }
            other => resolved_execution_type_at(other, contract, depth + 1),
        },
        ResolvedCanonicalType::List(_) => Ok(Type::List),
        // A by-value named `class` is embedded in its parent row, so it lowers to
        // a schema-identified record rather than a key reference.
        ResolvedCanonicalType::User { type_id, .. } => by_value_user_type(contract, *type_id),
        ResolvedCanonicalType::Generic { .. } => {
            Err(Error::Unsupported("resolved VOS generic type is not in the execution slice"))
        }
    }
}

/// Lowers a user-defined type used by value into an execution type.
///
/// A `class` is a named domain value embedded in its parent row, so the
/// execution type is a [`Type::Record`] keyed by the class's durable type id.
/// A `structure` is the same shape with a closed field set. A `unite` value is
/// a record-shaped handle in the operator model, while the row codec stores its
/// stable discriminant and payload from the catalog's variant table.
/// `enums` and `flags` are numeric: the value is one declared number or a mask
/// of declared bits, so the execution type is `i64` and the catalog's member
/// table keeps the named identity. A `table` has no by-value form: it is only
/// reachable through `&Table`, which stores the referenced primary key, so a
/// bare table reference is refused.
fn by_value_user_type(contract: &schema_types::ResolvedContract, type_id: u64) -> Result<Type> {
    let target = contract
        .types
        .iter()
        .find(|entry| entry.type_id == type_id)
        .ok_or_else(|| Error::Schema { message: format!("resolved user type targets unknown type id `{type_id}`") })?;
    match target.kind {
        schema_types::TypeContractKind::Class | schema_types::TypeContractKind::Structure => {
            let field_count = u32::try_from(target.fields.len())
                .map_err(|_| Error::Schema { message: format!("resolved type `{}` has too many fields", target.canonical_path.join("::")) })?;
            Ok(Type::Record { schema_id: type_id, field_count })
        }
        schema_types::TypeContractKind::Unite => {
            let variant_count = u32::try_from(target.variants.len())
                .map_err(|_| Error::Schema { message: format!("resolved unite `{}` has too many variants", target.canonical_path.join("::")) })?;
            Ok(Type::Record { schema_id: type_id, field_count: variant_count })
        }
        schema_types::TypeContractKind::Enums | schema_types::TypeContractKind::Flags => Ok(Type::I64),
        schema_types::TypeContractKind::Table => Err(Error::Schema {
            message: format!(
                "resolved type `{}` is a table and must be referenced as `&{}`",
                target.canonical_path.join("::"),
                target.canonical_path.last().map(String::as_str).unwrap_or_default()
            ),
        }),
    }
}

/// Deepest `Optional` / `Reference` nesting the catalog follows.
const MAX_TYPE_DEPTH: u8 = 32;

/// Primary-key type of the table a reference points at.
fn reference_key_type(contract: &schema_types::ResolvedContract, type_id: u64) -> Result<&schema_types::ResolvedCanonicalType> {
    let target = contract
        .types
        .iter()
        .find(|entry| entry.type_id == type_id)
        .ok_or_else(|| Error::Schema { message: format!("resolved reference targets unknown type id `{type_id}`") })?;
    let field = target.fields.iter().find(|field| field.attributes.iter().any(|attribute| attribute.name == "primary")).ok_or_else(|| {
        Error::Schema { message: format!("resolved reference target `{}` has no primary key field", target.canonical_path.join("::")) }
    })?;
    Ok(&field.canonical_type)
}

/// Maps a canonical VOS scalar to the exact 本地 execution scalar.
///
/// Only scalars whose storage semantics 本地 already owns are accepted. Narrow
/// integers, `f32`, `utf16` and wider integers have no execution type here, so
/// they return [`Error::Schema`] instead of widening, merging or silently
/// dropping the declared scalar. `f64` is accepted because the row codec and
/// coercion path already carry [`Value::F64`].
pub fn execution_type_for_scalar(scalar: schema_types::CanonicalScalar) -> Result<Type> {
    use schema_types::CanonicalScalar;
    match scalar {
        CanonicalScalar::I64 => Ok(Type::I64),
        CanonicalScalar::F64 => Ok(Type::F64),
        CanonicalScalar::Bool => Ok(Type::Bool),
        CanonicalScalar::Utf8 => Ok(Type::Text),
        CanonicalScalar::Bytes => Ok(Type::Bytes),
        CanonicalScalar::Date => Ok(Type::Date),
        CanonicalScalar::Time => Ok(Type::Time),
        CanonicalScalar::DateTime => Ok(Type::Timestamp),
        CanonicalScalar::Uuid => Ok(Type::Uuid),
        CanonicalScalar::Decimal => Ok(Type::Decimal { precision: database_types::scalar::DECIMAL_MAX_PRECISION, scale: 0 }),
        other => Err(Error::Schema { message: format!("VOS scalar `{other}` is not in the local execution scalar set") }),
    }
}

/// Maps a 本地 execution scalar back to its canonical VOS scalar.
///
/// Precision and scale are catalog metadata of [`Type::Decimal`], so the
/// canonical form is always [`schema_types::CanonicalScalar::Decimal`] and the
/// declared shape is validated rather than encoded. Composite execution types
/// (files, vectors, records, null) have no scalar spelling and return
/// [`Error::Schema`].
pub fn canonical_scalar_for_execution_type(ty: &Type) -> Result<schema_types::CanonicalScalar> {
    use schema_types::CanonicalScalar;
    match ty {
        Type::I64 => Ok(CanonicalScalar::I64),
        Type::F64 => Ok(CanonicalScalar::F64),
        Type::Bool => Ok(CanonicalScalar::Bool),
        Type::Text => Ok(CanonicalScalar::Utf8),
        Type::Bytes => Ok(CanonicalScalar::Bytes),
        Type::Date => Ok(CanonicalScalar::Date),
        Type::Time => Ok(CanonicalScalar::Time),
        Type::Timestamp => Ok(CanonicalScalar::DateTime),
        Type::Uuid => Ok(CanonicalScalar::Uuid),
        Type::Decimal { precision, scale } => {
            database_types::Decimal::new(0, *precision, *scale)
                .map_err(|error| Error::Schema { message: format!("execution decimal shape ({precision}, {scale}) is invalid: {error}") })?;
            Ok(CanonicalScalar::Decimal)
        }
        other => Err(Error::Schema { message: format!("execution type {other:?} has no canonical VOS scalar") }),
    }
}

/// Reads `Decimal<precision, scale>` literal arguments into the execution type.
fn decimal_type_from_arguments(arguments: &[schema_types::ResolvedCanonicalTypeArgument]) -> Result<Type> {
    use schema_types::ResolvedCanonicalTypeArgument;
    let [ResolvedCanonicalTypeArgument::Literal(precision), ResolvedCanonicalTypeArgument::Literal(scale)] = arguments
    else {
        return Err(Error::Schema { message: "`Decimal` requires two literal arguments: precision and scale".into() });
    };
    let precision = precision
        .parse::<u8>()
        .map_err(|_| Error::Schema { message: format!("`Decimal` precision `{precision}` is not an integer in 1..=38") })?;
    let scale = scale.parse::<u8>().map_err(|_| Error::Schema { message: format!("`Decimal` scale `{scale}` is not an integer") })?;
    database_types::Decimal::new(0, precision, scale)
        .map(|decimal| Type::Decimal { precision: decimal.precision(), scale: decimal.scale() })
        .map_err(|error| Error::Schema { message: format!("`Decimal<{precision}, {scale}>` rejected: {error}") })
}

/// Builds the row codec's named-layout directory from one resolved contract.
///
/// The directory is the only place the physical codec learns a `unite` variant's
/// discriminant and a member's durable identity. Columns are keyed by display
/// name for the in-memory row map, while stored records carry `FieldId`s; named
/// declarations are keyed by durable schema identity.
///
/// A layout revision is derived from what the stored bytes depend on: durable
/// type/field identities and type shapes, never display names or declaration
/// order. Renaming or reordering therefore keeps the revision, while adding,
/// dropping or retyping a member publishes a new one.
pub fn row_schema_directory(contract: &schema_types::ResolvedContract) -> Result<RowSchemaDirectory> {
    let mut named = BTreeMap::new();
    for item in &contract.types {
        let layout = match item.kind {
            schema_types::TypeContractKind::Class => {
                NamedValueLayout::Class { schema_id: item.type_id, fields: named_fields(&item.fields, contract, 0) }
            }
            schema_types::TypeContractKind::Structure => NamedValueLayout::Structure {
                schema_id: item.type_id,
                revision: structure_revision(item),
                fields: named_fields(&item.fields, contract, 0),
            },
            schema_types::TypeContractKind::Unite => NamedValueLayout::Unite {
                schema_id: item.type_id,
                revision: unite_revision(item),
                variants: item
                    .variants
                    .iter()
                    .map(|variant| NamedVariantLayout {
                        discriminant: variant.discriminant,
                        fields: variant
                            .fields
                            .iter()
                            .map(|field| NamedFieldLayout {
                                // Variant payload members do not carry a durable
                                // field identity yet; the physical order stays
                                // positional and is covered by the revision.
                                field_id: 0,
                                name: field.canonical_name.clone(),
                                ty: value_layout(&field.canonical_type, contract, 0),
                            })
                            .collect(),
                    })
                    .collect(),
            },
            _ => continue,
        };
        named.insert(item.type_id, layout);
    }
    let mut columns = BTreeMap::new();
    let mut column_ids = BTreeMap::new();
    for item in &contract.types {
        if item.kind != schema_types::TypeContractKind::Table {
            continue;
        }
        for field in &item.fields {
            let layout = value_layout(&field.canonical_type, contract, 0);
            if matches!(layout, ValueLayout::Dynamic) {
                continue;
            }
            if let Some(existing) = columns.get(&field.canonical_name)
                && existing != &layout
            {
                // The in-memory row map is name-keyed, so two different named
                // layouts for one column name cannot be encoded unambiguously.
                return Err(Error::Schema {
                    message: format!(
                        "column `{}` is declared with different named layouts in this contract; the row codec needs one \
                         layout per column name",
                        field.canonical_name
                    ),
                });
            }
            columns.insert(field.canonical_name.clone(), layout);
            column_ids.insert(field.canonical_name.clone(), field.field_id);
        }
    }
    Ok(RowSchemaDirectory::revisioned(row_schema_revision(contract), columns, column_ids, named))
}

/// Structural revision of one contract's row layout.
///
/// Only durable identities and type shapes participate, so a rename or a
/// declaration reorder keeps the revision and every stored byte readable.
pub fn row_schema_revision(contract: &schema_types::ResolvedContract) -> u32 {
    let mut hash = Fnv1a::default();
    let mut types: Vec<&schema_types::ResolvedTypeContract> = contract.types.iter().collect();
    types.sort_by_key(|item| item.type_id);
    for item in types {
        hash.write(&item.type_id.to_le_bytes());
        let mut fields: Vec<&schema_types::ResolvedFieldContract> = item.fields.iter().collect();
        fields.sort_by_key(|field| field.field_id);
        for field in fields {
            hash.write(&field.field_id.to_le_bytes());
            hash.write(type_signature(&field.canonical_type).as_bytes());
        }
        let mut variants: Vec<&schema_types::ResolvedVariantContract> = item.variants.iter().collect();
        variants.sort_by_key(|variant| variant.discriminant);
        for variant in variants {
            hash.write(&variant.discriminant.to_le_bytes());
            // Unite payload fields do not carry a durable field identity yet, so
            // their name and type still participate in the layout revision.
            for field in &variant.fields {
                hash.write(field.canonical_name.as_bytes());
                hash.write(type_signature(&field.canonical_type).as_bytes());
            }
        }
        hash.write(b"\x1f");
    }
    hash.finish()
}

fn named_fields(
    fields: &[schema_types::ResolvedFieldContract],
    contract: &schema_types::ResolvedContract,
    depth: u8,
) -> Vec<NamedFieldLayout> {
    fields
        .iter()
        .map(|field| NamedFieldLayout {
            field_id: field.field_id,
            name: field.canonical_name.clone(),
            ty: value_layout(&field.canonical_type, contract, depth),
        })
        .collect()
}

fn value_layout(ty: &schema_types::ResolvedCanonicalType, contract: &schema_types::ResolvedContract, depth: u8) -> ValueLayout {
    use schema_types::ResolvedCanonicalType;
    if depth > MAX_TYPE_DEPTH {
        return ValueLayout::Dynamic;
    }
    match ty {
        ResolvedCanonicalType::Optional(inner) => value_layout(inner, contract, depth + 1),
        // A reference stores the target's primary key, so it has no named value.
        ResolvedCanonicalType::Reference(_) => ValueLayout::Dynamic,
        ResolvedCanonicalType::List(inner) => ValueLayout::List(Box::new(value_layout(inner, contract, depth + 1))),
        ResolvedCanonicalType::User { type_id, .. } => {
            let named = contract.types.iter().any(|entry| {
                entry.type_id == *type_id
                    && matches!(
                        entry.kind,
                        schema_types::TypeContractKind::Class
                            | schema_types::TypeContractKind::Structure
                            | schema_types::TypeContractKind::Unite
                    )
            });
            if named {
                ValueLayout::Named(*type_id)
            }
            else {
                ValueLayout::Dynamic
            }
        }
        _ => ValueLayout::Dynamic,
    }
}

fn structure_revision(item: &schema_types::ResolvedTypeContract) -> u32 {
    let mut hash = Fnv1a::default();
    let mut fields: Vec<&schema_types::ResolvedFieldContract> = item.fields.iter().collect();
    fields.sort_by_key(|field| field.field_id);
    for field in fields {
        hash.write(&field.field_id.to_le_bytes());
        hash.write(type_signature(&field.canonical_type).as_bytes());
    }
    hash.finish()
}

fn unite_revision(item: &schema_types::ResolvedTypeContract) -> u32 {
    let mut hash = Fnv1a::default();
    let mut variants: Vec<&schema_types::ResolvedVariantContract> = item.variants.iter().collect();
    variants.sort_by_key(|variant| variant.discriminant);
    for variant in variants {
        hash.write(&variant.discriminant.to_le_bytes());
        for field in &variant.fields {
            hash.write(field.canonical_name.as_bytes());
            hash.write(type_signature(&field.canonical_type).as_bytes());
        }
        // A separator keeps `[a, b]` distinct from `[a b]`.
        hash.write(b"\x1f");
    }
    hash.finish()
}

pub(crate) fn type_signature(ty: &schema_types::ResolvedCanonicalType) -> String {
    use schema_types::{ResolvedCanonicalType, ResolvedCanonicalTypeArgument};
    match ty {
        ResolvedCanonicalType::Scalar(scalar) => scalar.to_string(),
        ResolvedCanonicalType::File => "file".to_owned(),
        ResolvedCanonicalType::Vector { element, dimension } => format!("vector<{element:?},{dimension}>"),
        ResolvedCanonicalType::User { type_id, .. } => format!("user:{type_id}"),
        ResolvedCanonicalType::Reference(inner) => format!("&{}", type_signature(inner)),
        ResolvedCanonicalType::Optional(inner) => format!("?{}", type_signature(inner)),
        ResolvedCanonicalType::List(inner) => format!("[]{}", type_signature(inner)),
        ResolvedCanonicalType::Generic { path, arguments } => {
            let arguments = arguments
                .iter()
                .map(|argument| match argument {
                    ResolvedCanonicalTypeArgument::Type(ty) => type_signature(ty),
                    ResolvedCanonicalTypeArgument::Literal(literal) => literal.clone(),
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{}<{arguments}>", path.join("::"))
        }
    }
}

/// FNV-1a over the layout signature, used only as a stable revision number.
struct Fnv1a(u32);

impl Default for Fnv1a {
    fn default() -> Self {
        Self(0x811c_9dc5)
    }
}

impl Fnv1a {
    fn write(&mut self, bytes: &[u8]) {
        let mut hash = self.0;
        for byte in bytes {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
        self.0 = hash;
    }

    fn finish(&self) -> u32 {
        self.0
    }
}

#[cfg(test)]
mod row_schema_tests {
    use super::*;

    fn resolve(document: &str) -> schema_types::ResolvedContract {
        let projection = crate::engine::schema_vos::project_schema(document).expect("project schema");
        let manifest = crate::engine::schema_vos::allocate_initial_manifest(&projection);
        let bound = crate::engine::schema_vos::bind(&projection, &manifest).expect("bind manifest");
        let history = crate::engine::schema_vos::evolve(&crate::engine::schema_vos::empty_history(), bound).expect("evolve");
        crate::engine::schema_vos::resolve_contract(&history.snapshot).expect("resolve contract")
    }

    fn declaration<'a>(
        directory: &'a RowSchemaDirectory,
        contract: &schema_types::ResolvedContract,
        name: &str,
    ) -> &'a NamedValueLayout {
        let item = contract
            .types
            .iter()
            .find(|entry| entry.canonical_path.last().map(String::as_str) == Some(name))
            .unwrap_or_else(|| panic!("`{name}` must resolve"));
        directory.named(item.type_id).unwrap_or_else(|| panic!("`{name}` must have a layout"))
    }

    fn unite_revision(document: &str) -> u32 {
        let contract = resolve(document);
        match declaration(&row_schema_directory(&contract).expect("directory"), &contract, "Contact") {
            NamedValueLayout::Unite { revision, .. } => *revision,
            other => panic!("expected a union layout, got {other:?}"),
        }
    }

    fn structure_revision(document: &str) -> u32 {
        let contract = resolve(document);
        match declaration(&row_schema_directory(&contract).expect("directory"), &contract, "Address") {
            NamedValueLayout::Structure { revision, .. } => *revision,
            other => panic!("expected a structure layout, got {other:?}"),
        }
    }

    #[test]
    fn f64_scalars_map_to_the_real_execution_type() {
        assert_eq!(execution_type_for_scalar(schema_types::CanonicalScalar::F64).unwrap(), Type::F64);
        assert_eq!(canonical_scalar_for_execution_type(&Type::F64).unwrap(), schema_types::CanonicalScalar::F64);
    }

    #[test]
    fn a_unite_rename_keeps_the_layout_revision_and_a_reorder_changes_it() {
        let base = unite_revision("unite Contact { Email { address: utf8 }, Sms { number: utf8 }, None }");
        let renamed = unite_revision("unite Contact { Postal { address: utf8 }, Sms { number: utf8 }, None }");
        let reordered = unite_revision("unite Contact { Sms { number: utf8 }, Email { address: utf8 }, None }");
        let widened = unite_revision("unite Contact { Email { address: utf8, label: utf8? }, Sms { number: utf8 }, None }");
        assert_eq!(base, renamed, "renaming a variant keeps its source-order discriminant and revision");
        assert_ne!(base, reordered, "reordering variants shifts discriminants, so stale bytes must reject");
        assert_ne!(base, widened, "changing a payload shape changes the layout revision");
    }

    #[test]
    fn a_structure_revision_tracks_member_order_and_shape() {
        let base = structure_revision("structure Address { zip: utf8, country: utf8? }");
        let reordered = structure_revision("structure Address { country: utf8?, zip: utf8 }");
        let widened = structure_revision("structure Address { zip: utf8, country: utf8?, district: utf8? }");
        assert_ne!(base, reordered, "positional layouts bind to declaration order");
        assert_ne!(base, widened, "adding a member changes the positional layout");
    }

    #[test]
    fn unite_discriminants_follow_declaration_order() {
        let contract = resolve("unite Contact { Email { address: utf8 }, Sms { number: utf8 }, None }");
        let directory = row_schema_directory(&contract).expect("directory");
        let NamedValueLayout::Unite { variants, .. } = declaration(&directory, &contract, "Contact")
        else {
            panic!("expected a union layout");
        };
        assert_eq!(
            variants.iter().map(|variant| (variant.discriminant, variant.fields.len())).collect::<Vec<_>>(),
            [(0, 1), (1, 1), (2, 0)]
        );
    }
}
