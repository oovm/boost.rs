//! Source-level write helpers for the local integration tests.
//!
//! The execution crate no longer parses VOS writes: tests parse through the
//! frontend and hand the lowered [`database_query::local::WriteOp`] plan to the
//! shared operator executor, which is exactly what the connection does.

use std::collections::BTreeMap;

use database_execution::engine::bytes::Placement;
use database_execution::local::execute_ops;
use database_query::local::{constraints, lower_write_program, point_write_key};
use database_types::{Error, Result, TableUniqueConstraint, Value};
use schema_types::ResolvedContract;

pub fn parse(source: &str) -> Result<schema_input_vos::syntax::VosProgram> {
    schema_input_vos::parse_program(source).map_err(|diagnostics| Error::Schema { message: diagnostics })
}

pub fn execute_write(
    source: &str,
    contract: &ResolvedContract,
    document: Option<&str>,
    records: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    execute_write_with_params(source, contract, &[], document, records, &[])
}

pub fn execute_write_with_constraints(
    source: &str,
    contract: &ResolvedContract,
    declared: &[TableUniqueConstraint],
    document: Option<&str>,
    records: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    execute_write_with_params(source, contract, declared, document, records, &[])
}

pub fn execute_write_with_params(
    source: &str,
    contract: &ResolvedContract,
    declared: &[TableUniqueConstraint],
    document: Option<&str>,
    records: &mut BTreeMap<String, Vec<u8>>,
    params: &[Value],
) -> Result<()> {
    let program = parse(source)?;
    let mut place = |value| Ok(value);
    let ops = lower_write_program(&program, contract, document, params, &mut place)?;
    let merged = constraints::merge(constraints::from_contract(contract)?, declared);
    execute_ops(&ops, contract, &merged, records, params, &Placement::Inline, None)?;
    Ok(())
}

pub fn point_write_keys(
    source: &str,
    contract: &ResolvedContract,
    declared: &[TableUniqueConstraint],
    document: Option<&str>,
    params: &[Value],
    holds_every_row: bool,
) -> Result<Option<Vec<String>>> {
    let program = parse(source)?;
    let mut place = |value| Ok(value);
    let merged = constraints::merge(constraints::from_contract(contract)?, declared);
    let Ok(ops) = lower_write_program(&program, contract, document, params, &mut place)
    else {
        return Ok(None);
    };
    let mut keys = Vec::with_capacity(ops.len());
    for op in &ops {
        let Some(key) = point_write_key(op, contract, &merged, holds_every_row)? else {
            return Ok(None);
        };
        keys.push(key);
    }
    keys.sort();
    keys.dedup();
    Ok(Some(keys))
}
