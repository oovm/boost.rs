use crate::engine::{Error, Result, SchemaVersion, catalog as schema, kv::retention as ttl, schema_vos};

use super::Connection;

impl Connection {
    /// Current database-truth schema, if any.
    pub fn schema(&self) -> Result<Option<SchemaVersion>> {
        Ok(self.read_state()?.schema)
    }

    /// Current persisted VOS identity history, if the database has one.
    pub fn identity_history(&self) -> Result<Option<schema_types::IdentityHistory>> {
        Ok(self.read_state()?.identity_history)
    }

    /// Current validated VOS resolved contract, if the database has one.
    pub fn resolved_contract(&self) -> Result<Option<std::sync::Arc<schema_types::ResolvedContract>>> {
        Ok(self.read_state()?.resolved_contract)
    }

    /// Bind local execution handles using persisted identities, never fresh source order.
    pub fn execution_catalog(&self) -> Result<Option<schema::ExecutionCatalog>> {
        let state = self.read_state()?;
        let Some(contract) = state.resolved_contract
        else {
            let Some(schema) = state.schema
            else {
                return Ok(None);
            };
            self.ensure_schema(&schema.document)?;
            let state = self.read_state()?;
            let Some(contract) = state.resolved_contract
            else {
                return Ok(None);
            };
            let mut catalog = schema::execution_catalog_from_resolved_contract(&contract)?;
            catalog.apply_declared_unique_constraints(&state.unique_constraints)?;
            return Ok(Some(catalog));
        };
        let mut catalog = schema::execution_catalog_from_resolved_contract(&contract)?;
        catalog.apply_declared_unique_constraints(&state.unique_constraints)?;
        Ok(Some(catalog))
    }

    /// Persisted host-declared uniqueness rules, in declaration order.
    pub fn unique_constraints(&self) -> Result<Vec<database_types::TableUniqueConstraint>> {
        Ok(self.read_state()?.unique_constraints)
    }

    /// Declares a persisted uniqueness rule over existing table columns.
    ///
    /// VOS `@` (`unique`) attributes already produce single-column rules. This
    /// entry point adds composite column sets and lets hosts choose the
    /// constraint name. A declaration with the same table and name replaces the
    /// previous one. Names derived from the VOS document are reserved.
    pub fn declare_unique_constraint(&self, table: &str, constraint: database_types::UniqueConstraint) -> Result<()> {
        let state = self.read_state()?;
        if state.resolved_contract.is_none() {
            if schema_document(&state).is_none() {
                return Err(Error::Schema { message: "declare_unique_constraint requires an installed schema".into() });
            }
            if let Some(schema) = schema_document(&state) {
                self.ensure_schema(&schema)?;
            }
        }
        let _guard = self.operation_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = self.read_state()?;
        let contract = state
            .resolved_contract
            .as_ref()
            .ok_or_else(|| Error::Schema { message: "declare_unique_constraint requires a resolved schema contract".into() })?;
        let catalog = schema::execution_catalog_from_resolved_contract(contract)?;
        let entry = catalog
            .types
            .iter()
            .find(|entry| entry.kind == schema::ExecutionTypeKind::Table && entry.name == table)
            .ok_or_else(|| Error::Schema { message: format!("unique constraint refers to unknown table `{table}`") })?;
        for column in constraint.columns() {
            if !entry.fields.iter().any(|field| &field.name == column) {
                return Err(Error::Schema { message: format!("unique constraint refers to unknown column `{column}` on table `{table}`") });
            }
        }
        if catalog.has_unique_constraint(table, constraint.name()) {
            return Err(Error::Schema {
                message: format!("unique constraint name `{}` is reserved by the VOS document on table `{table}`", constraint.name()),
            });
        }
        let declared = database_types::TableUniqueConstraint::new(table, constraint).map_err(Error::from)?;
        state
            .unique_constraints
            .retain(|existing| !(existing.table() == declared.table() && existing.constraint().name() == declared.constraint().name()));
        state.unique_constraints.push(declared);
        // A new rule has no cached entries yet; the next full write rebuilds.
        self.unique_index.invalidate_all();
        self.write_state(&state)?;
        Ok(())
    }

    /// Number of stored key/value records.
    pub fn record_count(&self) -> Result<usize> {
        Ok(self.read_state()?.records.keys().filter(|key| !ttl::is_reserved_key(key)).count())
    }

    /// Current persisted schema version, if any.
    pub fn schema_version(&self) -> Result<Option<u32>> {
        Ok(self.schema()?.map(|schema| schema.version))
    }

    /// Installs a schema made of several source documents as one contract.
    ///
    /// Each document carries its own `namespace`, so joining them lets a reference such
    /// as `&oai_autos::User` resolve against the other document instead of failing as an
    /// unknown type. The joined text is the persisted schema document, so a later
    /// `ensure_schema` with the same list must produce the same bytes.
    pub fn ensure_schema_files(&self, documents: &[&str]) -> Result<()> {
        let joined = join_schema_documents(documents);
        self.ensure_schema(&joined)
    }

    /// Evolves a multi-document schema to the next version.
    pub fn migrate_schema_files(&self, documents: &[&str], renames: &schema::RenameMap) -> Result<()> {
        let joined = join_schema_documents(documents);
        self.migrate_schema(&joined, renames)
    }

    /// Persist the database-truth VOS schema on first use, then verify later
    /// calls structurally against the durable catalog.
    ///
    /// The initial version is `1`, or `// @yydb-schema-version: <n>` in leading
    /// comments. Once a schema is persisted, a document that rebinds to the
    /// same durable identities is accepted even when its formatting differs —
    /// the stored text then adopts the new spelling. A document that changes
    /// meaning must go through [`Self::migrate_schema`] to advance the version.
    pub fn ensure_schema(&self, document: &str) -> Result<()> {
        let projection = schema_vos::project_schema(document)?;
        let _guard = self.operation_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = self.read_state()?;
        if let Some(current) = &state.schema {
            let resolved = rebind_identities(&state, &projection)?;
            let mut dirty = false;
            if state.resolved_contract.is_none() {
                state.resolved_contract = Some(std::sync::Arc::new(resolved));
                dirty = true;
            }
            if current.document != document {
                // Same durable identities, different spelling: adopt the new text so
                // stored-document fingerprints match what callers keep passing in.
                state.schema = Some(SchemaVersion { version: current.version, document: document.to_owned() });
                dirty = true;
            }
            if dirty {
                self.write_state(&state)?;
            }
            return Ok(());
        }
        let version = schema_vos::initial_version(document);
        let manifest = schema_vos::allocate_initial_manifest(&projection);
        let bound = schema_vos::bind(&projection, &manifest)?;
        let history = schema_vos::evolve(&schema_vos::empty_history(), bound)?;
        let contract = schema_vos::resolve_contract(&history.snapshot)?;
        self.install_catalog_unlocked(&mut state, CatalogDefinition::new(contract, history, version, Some(document.to_owned())))
    }

    /// Evolve the persisted schema to the next version (`current + 1`).
    ///
    /// Existing type and field identities are preserved by exact name matching
    /// or by mappings in `renames`. When the document carries
    /// `// @yydb-schema-version: <n>`, `n` must equal the new version.
    pub fn migrate_schema(&self, document: &str, renames: &schema::RenameMap) -> Result<()> {
        let projection = schema_vos::project_schema(document)?;
        let _guard = self.operation_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = self.read_state()?;
        let current = state.schema.as_ref().ok_or_else(|| Error::Schema { message: "schema migration requires an existing schema".into() })?;
        let version = current.version + 1;
        schema_vos::validate_migration_version(document, version)?;
        let history = state
            .identity_history
            .as_ref()
            .ok_or_else(|| Error::Schema { message: "initialize the identity ledger with ensure_schema before migration".into() })?;
        let manifest = schema_vos::migration_manifest(history, &projection, renames)?;
        let bound = schema_vos::bind(&projection, &manifest)?;
        let evolved = schema_vos::evolve(history, bound)?;
        let resolved_contract = schema_vos::resolve_contract(&evolved.snapshot)?;
        // A field that keeps its durable identity must keep its declared type.
        // Only an explicit rename declares that the caller owns the change, so
        // an implicit `i64 -> utf8` can never advance the version and leave the
        // stored rows describing a different type than the catalog.
        let previous = state.resolved_contract.clone();
        if let Some(previous) = previous.as_deref() {
            validate_retained_field_types(previous, &resolved_contract, renames)?;
            rewrite_renamed_rows(&mut state, previous, &resolved_contract, renames)?;
        }
        let remapped = schema::remap_unique_constraints(&state.unique_constraints, renames)?;
        state.unique_constraints = remapped;
        self.install_catalog_unlocked(&mut state, CatalogDefinition::new(resolved_contract, evolved, version, Some(document.to_owned())))
    }

    /// Publish a host-supplied catalog as the database truth.
    ///
    /// This is the text-free entry point: the resolved contract and its
    /// identity ledger arrive already built, so hosts (tooling, replication,
    /// catalogs derived from another dialect) never have to produce VOS
    /// source. The contract is validated and lowered against the persisted
    /// declared uniqueness rules before anything is written, so an unusable
    /// catalog fails without touching state. When `document` is absent, the
    /// stored schema text keeps its previous value and only the version
    /// changes; a first publication without text stores an empty document,
    /// which [`Self::ensure_schema`] later adopts when a VOS document rebinds
    /// to the same identities.
    pub fn install_catalog(&self, definition: CatalogDefinition) -> Result<()> {
        let _guard = self.operation_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = self.read_state()?;
        self.install_catalog_unlocked(&mut state, definition)
    }

    /// Shared publication tail of [`Self::install_catalog`], [`Self::ensure_schema`]
    /// and [`Self::migrate_schema`]. The caller must hold the operation lock.
    fn install_catalog_unlocked(&self, state: &mut super::State, definition: CatalogDefinition) -> Result<()> {
        let mut catalog = schema::execution_catalog_from_resolved_contract(&definition.contract)?;
        catalog.apply_declared_unique_constraints(&state.unique_constraints)?;
        let document = definition
            .document
            .or_else(|| state.schema.as_ref().map(|schema| schema.document.clone()))
            .unwrap_or_default();
        state.resolved_contract = Some(std::sync::Arc::new(definition.contract));
        state.identity_history = Some(definition.identity);
        state.schema = Some(SchemaVersion { version: definition.version, document });
        // Every rule derived from the old catalog is now stale, including a warm
        // unique-key index whose column names may have just been renamed.
        self.unique_index.invalidate_all();
        self.write_state(state)?;
        Ok(())
    }
}

/// A complete catalog publication for [`Connection::install_catalog`].
///
/// Carries the resolved contract the executor binds against, the durable
/// identity ledger that survives restarts, the schema version to publish and
/// the optional source document kept for fingerprints and diagnostics.
#[derive(Debug, Clone)]
pub struct CatalogDefinition {
    contract: schema_types::ResolvedContract,
    identity: schema_types::IdentityHistory,
    version: u32,
    document: Option<String>,
}

impl CatalogDefinition {
    /// Builds a publication from its parts.
    pub fn new(
        contract: schema_types::ResolvedContract,
        identity: schema_types::IdentityHistory,
        version: u32,
        document: Option<String>,
    ) -> Self {
        Self { contract, identity, version, document }
    }
}

fn schema_document(state: &super::State) -> Option<String> {
    state.schema.as_ref().map(|schema| schema.document.clone())
}

/// Rejects an implicit type change of a field that keeps its durable identity.
///
/// The durable field id is what stored rows and prepared plans bind to, so a
/// migration that silently reinterprets that identity under another type would
/// leave old bytes addressing a different declared type. Only an explicit
/// rename states that the caller owns the change; anything else fails before
/// the version advances.
fn validate_retained_field_types(
    previous: &schema_types::ResolvedContract,
    next: &schema_types::ResolvedContract,
    renames: &schema::RenameMap,
) -> Result<()> {
    for previous_type in &previous.types {
        let Some(next_type) = next.types.iter().find(|item| item.type_id == previous_type.type_id)
        else {
            continue;
        };
        let previous_name = previous_type.canonical_path.last().map(String::as_str).unwrap_or_default();
        for previous_field in &previous_type.fields {
            let Some(next_field) = next_type.fields.iter().find(|field| field.field_id == previous_field.field_id)
            else {
                continue;
            };
            let before = schema::type_signature(&previous_field.canonical_type);
            let after = schema::type_signature(&next_field.canonical_type);
            if before == after || renames.field_target(previous_name, &previous_field.canonical_name).is_some() {
                continue;
            }
            return Err(Error::Schema {
                message: format!(
                    "migration changes the type of field `{previous_name}.{}` from `{before}` to `{after}` without an \
                     explicit rename; declare the field migration instead of reusing its durable identity",
                    previous_field.canonical_name
                ),
            });
        }
    }
    Ok(())
}

/// Rewrites stored row records so renamed columns and tables keep their data.
///
/// A stored row carries its column names, and a renamed table owns a different
/// record-key prefix, so publishing the new catalog alone would leave every old
/// row addressing the previous names. Both rewrites are prepared in memory and
/// only applied after every record decoded and re-encoded successfully, so a
/// failure leaves the rows exactly as they were.
fn rewrite_renamed_rows(
    state: &mut super::State,
    previous: &schema_types::ResolvedContract,
    next: &schema_types::ResolvedContract,
    renames: &schema::RenameMap,
) -> Result<()> {
    use database_formats::yy::row::{decode_row_with_directory, encode_row_with_directory, table_prefix};
    use schema_types::TypeContractKind;

    let old_directory = schema::row_schema_directory(previous)?;
    let new_directory = schema::row_schema_directory(next)?;
    let mut rewritten = Vec::new();
    for previous_type in previous.types.iter().filter(|item| item.kind == TypeContractKind::Table) {
        let Some(next_type) = next.types.iter().find(|item| item.type_id == previous_type.type_id && item.kind == TypeContractKind::Table)
        else {
            continue;
        };
        let previous_name = previous_type.canonical_path.last().map(String::as_str).unwrap_or_default();
        let next_name = next_type.canonical_path.last().map(String::as_str).unwrap_or_default();
        let old_table = database_query::local::catalog::table_row_key_name(previous, previous_name)?;
        let new_table = database_query::local::catalog::table_row_key_name(next, next_name)?;
        let mut field_renames = std::collections::BTreeMap::new();
        for field in &previous_type.fields {
            let target = renames.field_target(previous_name, &field.canonical_name).unwrap_or(field.canonical_name.as_str());
            if target != field.canonical_name {
                field_renames.insert(field.canonical_name.clone(), target.to_owned());
            }
        }
        if old_table == new_table && field_renames.is_empty() {
            continue;
        }
        let prefix = table_prefix(&old_table);
        let next_prefix = table_prefix(&new_table);
        for (key, value) in state.records.range(prefix.clone()..).take_while(|(key, _)| key.starts_with(&prefix)) {
            let mut row = decode_row_with_directory(value, &old_directory)?;
            if !field_renames.is_empty() {
                let mut renamed = std::collections::BTreeMap::new();
                for (name, value) in std::mem::take(&mut row) {
                    let name = field_renames.get(&name).cloned().unwrap_or(name);
                    renamed.insert(name, value);
                }
                row = renamed;
            }
            // `prefix` is a whole record-key segment, so its remainder is the
            // encoded primary key and moves to the new table unchanged.
            let next_key = format!("{next_prefix}{}", &key[prefix.len()..]);
            rewritten.push((key.clone(), next_key, row));
        }
    }
    for (old_key, next_key, row) in rewritten {
        let bytes = encode_row_with_directory(&row, &new_directory)?;
        if old_key != next_key {
            state.records.remove(&old_key);
        }
        state.records.insert(next_key, bytes);
    }
    Ok(())
}

/// Joins schema documents into one source text.
///
/// A document may declare its own `namespace`, and the projection switches namespace at
/// each declaration, so concatenation keeps every type under its own namespace without
/// rewriting any path.
fn join_schema_documents(documents: &[&str]) -> String {
    documents.iter().map(|document| document.trim_end()).filter(|document| !document.is_empty()).collect::<Vec<_>>().join("\n\n")
}

/// Reproject a document against persisted identities and verify the ledger.
///
/// Returns the resolved contract for the persisted snapshot. A document that
/// no longer rebinds to the durable identities fails with [`Error::Schema`]
/// without rewriting any state; a database whose identity ledger is missing
/// stays a corruption error, because no caller document can explain it away.
fn rebind_identities(state: &super::State, projection: &schema_types::SchemaProjection) -> Result<schema_types::ResolvedContract> {
    let history = state.identity_history.as_ref().ok_or(Error::Corrupt("catalog identity ledger does not match schema"))?;
    let manifest = schema_vos::manifest_from_snapshot(&history.snapshot);
    let bound = schema_vos::bind(projection, &manifest).map_err(|_| schema_mismatch())?;
    if bound != history.snapshot {
        return Err(schema_mismatch());
    }
    schema_vos::resolve_contract(&bound)
}

fn schema_mismatch() -> Error {
    Error::Schema {
        message: "schema document changed -- call migrate_schema to advance the version".into(),
    }
}
