//! argv parsing: permissive global peeling, strict command parsing, and the
//! machine-path option configuration pass.

use std::collections::{BTreeMap, BTreeSet};

use commander_types::{CommanderError, error_id};
use serde_json::Value;

use crate::spec::{OptionSpec, ValueDecl, validate_value};

/// Values collected from argv, keyed by option long name.
///
/// The stored shape mirrors the TypeScript twin: a flag or bare optional-value
/// token records `true`, a supplied value records a string, and a repeatable
/// option records an array of strings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedOptions {
    values: BTreeMap<String, Value>,
    positionals: Vec<String>,
}

impl ParsedOptions {
    /// Raw value for one option key, if collected.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// String value for one option key.
    pub fn str_value(&self, key: &str) -> Option<&str> {
        self.values.get(key).and_then(Value::as_str)
    }

    /// Boolean value for one option key.
    pub fn bool_value(&self, key: &str) -> Option<bool> {
        self.values.get(key).and_then(Value::as_bool)
    }

    /// String items of a repeatable option; a single string reads as one item.
    pub fn string_list(&self, key: &str) -> Vec<String> {
        match self.values.get(key) {
            Some(Value::Array(items)) => items.iter().map(value_to_string).collect(),
            Some(value) if value.is_string() || value.is_boolean() => vec![value_to_string(value)],
            _ => Vec::new(),
        }
    }

    /// Whether any value was collected for one option key.
    pub fn is_present(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    /// Positional tokens, including the passthrough tail after `--`.
    pub fn positionals(&self) -> &[String] {
        &self.positionals
    }

    pub(crate) fn insert(&mut self, key: String, value: Value) {
        self.values.insert(key, value);
    }

    pub(crate) fn merge_missing(&mut self, other: &ParsedOptions) {
        for (key, value) in &other.values {
            self.values.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }

    pub(crate) fn extend_positionals(&mut self, tail: Vec<String>) {
        self.positionals.extend(tail);
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// Result of one parse pass.
pub(crate) struct Parsed {
    pub options: ParsedOptions,
    /// Non-option tokens left over (command path, positionals, unknown flags
    /// in permissive mode).
    pub rest: Vec<String>,
    /// Long names explicitly supplied on the command line; defaults do not count.
    pub used: BTreeSet<String>,
}

/// Permissive peel used for global options: unknown flags and bare tokens stay
/// in `rest` so a later subcommand can consume them.
pub(crate) fn peel(argv: &[String], defs: &[OptionSpec]) -> Parsed {
    parse_core(argv, defs, false).expect("permissive parsing never rejects argv")
}

/// Strict parse used before dispatching to a command handler: unknown options
/// and missing values fail with stable usage errors.
pub(crate) fn strict(argv: &[String], defs: &[OptionSpec]) -> Result<Parsed, CommanderError> {
    parse_core(argv, defs, true)
}

fn parse_core(argv: &[String], defs: &[OptionSpec], strict_mode: bool) -> Result<Parsed, CommanderError> {
    let mut longs: BTreeMap<&str, &OptionSpec> = BTreeMap::new();
    let mut shorts: BTreeMap<char, &OptionSpec> = BTreeMap::new();
    for def in defs {
        longs.insert(def.long.as_str(), def);
        if let Some(c) = def.short {
            shorts.insert(c, def);
        }
    }

    let mut parsed = Parsed { options: ParsedOptions::default(), rest: Vec::new(), used: BTreeSet::new() };

    // Bare `-` is a stdin/stdout sentinel, not an option marker: `--out -`
    // equals `--out=-`. Later flags are never swallowed as values.
    let is_option_token = |token: &str| token.len() > 1 && token.starts_with('-');

    let mut i = 0usize;
    while i < argv.len() {
        let a = &argv[i];
        if a == "--" {
            // Strict parsing consumes the separator; permissive peeling keeps
            // `--` and its tail so dispatch can split head/tail and never
            // interpret tail tokens as options or help markers.
            if strict_mode {
                parsed.rest.extend(argv[i + 1..].iter().cloned());
            } else {
                parsed.rest.push(a.clone());
                parsed.rest.extend(argv[i + 1..].iter().cloned());
            }
            break;
        }
        if a.starts_with("--") {
            let body = &a[2..];
            let (long, attached) = match body.find('=') {
                Some(eq) => (&body[..eq], Some(body[eq + 1..].to_string())),
                None => (body, None),
            };
            let Some(def) = longs.get(long).copied() else {
                if strict_mode {
                    return Err(CommanderError::from_id(error_id::UNKNOWN_OPTION).with_arg("option", a.clone()));
                }
                parsed.rest.push(a.clone());
                i += 1;
                continue;
            };
            match def.value {
                ValueDecl::Flag => {
                    assign(&mut parsed, def, Value::Bool(true));
                }
                ValueDecl::Required | ValueDecl::OptionalValue => {
                    if let Some(value) = attached {
                        assign(&mut parsed, def, Value::String(value));
                    } else {
                        let next = argv.get(i + 1);
                        if let Some(n) = next.filter(|n| !is_option_token(n)) {
                            assign(&mut parsed, def, Value::String(n.clone()));
                            i += 2;
                            continue;
                        }
                        if def.value == ValueDecl::OptionalValue {
                            assign(&mut parsed, def, Value::Bool(true));
                        } else if strict_mode {
                            return Err(CommanderError::from_id(error_id::MISSING_OPTION_VALUE)
                                .with_arg("option", format!("--{long}")));
                        } else {
                            parsed.rest.push(a.clone());
                        }
                    }
                }
            }
            i += 1;
            continue;
        }
        if a.starts_with('-') && a.len() > 1 {
            // Short options are single characters; `-xy` never resolves and is
            // reported as unknown (strict) or left for later (permissive).
            let body = &a[1..];
            let def = match body.chars().count() {
                1 => body.chars().next().and_then(|c| shorts.get(&c).copied()),
                _ => None,
            };
            let Some(def) = def else {
                if strict_mode {
                    return Err(CommanderError::from_id(error_id::UNKNOWN_OPTION).with_arg("option", a.clone()));
                }
                parsed.rest.push(a.clone());
                i += 1;
                continue;
            };
            match def.value {
                ValueDecl::Flag => {
                    assign(&mut parsed, def, Value::Bool(true));
                }
                ValueDecl::Required | ValueDecl::OptionalValue => {
                    let next = argv.get(i + 1);
                    if let Some(n) = next.filter(|n| !is_option_token(n)) {
                        assign(&mut parsed, def, Value::String(n.clone()));
                        i += 2;
                        continue;
                    }
                    if def.value == ValueDecl::OptionalValue {
                        assign(&mut parsed, def, Value::Bool(true));
                    } else if strict_mode {
                        return Err(CommanderError::from_id(error_id::MISSING_OPTION_VALUE)
                            .with_arg("option", a.clone()));
                    } else {
                        parsed.rest.push(a.clone());
                    }
                }
            }
            i += 1;
            continue;
        }
        parsed.rest.push(a.clone());
        i += 1;
    }
    Ok(parsed)
}

fn assign(parsed: &mut Parsed, def: &OptionSpec, value: Value) {
    parsed.used.insert(def.long.clone());
    if def.repeatable {
        let entry = parsed.options.values.entry(def.long.clone()).or_insert_with(|| Value::Array(Vec::new()));
        let list = entry.as_array_mut().expect("repeatable entries are arrays");
        list.push(match value {
            Value::String(s) => Value::String(s),
            Value::Bool(b) => Value::String(b.to_string()),
            other => other,
        });
        return;
    }
    parsed.options.values.insert(def.long.clone(), value);
}

/// Applies declared constraints (default / required / enum / coerce) after a
/// strict parse, before any handler runs. Only explicitly supplied values are
/// re-validated, matching the TypeScript twin.
pub(crate) fn apply_config(
    defs: &[OptionSpec],
    options: &mut ParsedOptions,
    used: &BTreeSet<String>,
) -> Result<(), CommanderError> {
    for def in defs {
        let provided = used.contains(&def.long);
        if !provided {
            if let Some(default) = &def.default {
                options.insert(def.long.clone(), Value::String(default.clone()));
            }
        }
        let Some(value) = options.get(&def.long).cloned() else {
            if def.required {
                return Err(CommanderError::from_id(error_id::MISSING_REQUIRED_OPTION)
                    .with_arg("option", def.long.clone()));
            }
            continue;
        };
        if !provided {
            continue;
        }
        for item in strings_of(&value) {
            validate_value(def, &item).map_err(|mut err| {
                if err.id == error_id::OPTION_NOT_IN_ENUM {
                    err.args.insert("allowed".to_string(), def.enum_values.join(", "));
                }
                err
            })?;
        }
    }
    Ok(())
}

fn strings_of(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().map(value_to_string).collect(),
        Value::String(_) | Value::Bool(_) => vec![value_to_string(value)],
        _ => Vec::new(),
    }
}
