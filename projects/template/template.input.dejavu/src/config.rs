#![warn(missing_docs)]
//! DejaVu Configuration
//!
//! This crate provides configuration management for the DejaVu template engine.
//! Loading layers TOML files and `DEJAVU_*` environment variables over the serde
//! defaults, and `toml_edit` handles in-place TOML manipulation.

use serde::{Deserialize, Serialize};
use serde_json::{Map as TomlMap, Value as TomlValue};
use std::path::{Path, PathBuf};
use template_types::{DejavuError, DejavuResult};

/// Default `template_dir` value (used by serde when all config sources omit it).
fn default_template_dir() -> PathBuf {
    PathBuf::from("./templates")
}

/// Main DejaVu configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DejavuConfig {
    /// Template base directory
    #[serde(default = "default_template_dir")]
    pub template_dir: PathBuf,
}

impl Default for DejavuConfig {
    fn default() -> Self {
        Self { template_dir: default_template_dir() }
    }
}

impl DejavuConfig {
    /// Create a new configuration with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Load configuration from a TOML file
    pub fn load<P: Into<PathBuf>>(path: P) -> DejavuResult<Self> {
        let path = path.into();
        let text = std::fs::read_to_string(&path)?;
        let config =
            data_text_toml::from_str(&text).map_err(|e| DejavuError::InvalidConfig(format!("failed to parse {}: {e}", path.display())))?;
        Ok(config)
    }

    /// Load configuration from the default location.
    ///
    /// Uses `dejavu.toml` / `.dejavu.toml` (cwd) plus env vars prefixed with `DEJAVU_`.
    /// All sources absent → serde default kicks in (no error, returns `Self::default()`).
    /// Any source present but invalid (malformed TOML, wrong value type, …)
    /// → `DejavuError::InvalidConfig` propagated — never silently swallowed.
    pub fn load_default() -> DejavuResult<Self> {
        let mut merged = serde_json::to_value(Self::default())
            .map_err(|e| DejavuError::InvalidConfig(format!("failed to encode default configuration: {e}")))?;

        for candidate in ["dejavu.toml", ".dejavu.toml"] {
            let path = Path::new(candidate);
            if path.is_file() {
                merge_toml(&mut merged, read_toml(path)?);
            }
        }
        merge_env(&mut merged, "DEJAVU_");

        serde_json::from_value(merged).map_err(|e| DejavuError::InvalidConfig(format!("invalid configuration: {e}")))
    }

    /// Save configuration to a TOML file
    pub fn save<P: Into<PathBuf>>(&self, path: P) -> DejavuResult<()> {
        let path = path.into();
        let content = data_text_toml::to_string(self).map_err(|e| DejavuError::InvalidConfig(format!("failed to serialize config: {e}")))?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Edit configuration using toml_edit
    pub fn edit<P: Into<PathBuf>>(path: P, edit_fn: impl Fn(&mut toml_edit::Document<String>)) -> DejavuResult<()> {
        let path = path.into();
        let content = std::fs::read_to_string(&path)?;
        let mut doc = content
            .parse::<toml_edit::Document<String>>()
            .map_err(|e| DejavuError::InvalidConfig(format!("failed to parse TOML document: {e}")))?;
        edit_fn(&mut doc);
        std::fs::write(&path, doc.to_string())?;
        Ok(())
    }
}

/// Read a TOML document into a value tree.
fn read_toml(path: &Path) -> DejavuResult<TomlValue> {
    let text = std::fs::read_to_string(path)?;
    data_text_toml::from_str(&text).map_err(|e| DejavuError::InvalidConfig(format!("failed to parse {}: {e}", path.display())))
}

/// Deep-merge `overlay` into `base`: tables merge key by key, every other value replaces.
fn merge_toml(base: &mut TomlValue, overlay: TomlValue) {
    match (base, overlay) {
        (TomlValue::Object(base_table), TomlValue::Object(overlay_table)) => {
            for (key, value) in overlay_table {
                match base_table.get_mut(&key) {
                    Some(slot) if slot.is_object() && value.is_object() => merge_toml(slot, value),
                    Some(slot) => *slot = value,
                    None => {
                        base_table.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// Merge `prefix`-prefixed environment variables, lowercased and `.`-split into nested keys.
///
/// Values stay strings: the only keys this crate reads are paths, and string input keeps
/// `DEJAVU_TEMPLATE_DIR=123` a directory name rather than a number.
fn merge_env(base: &mut TomlValue, prefix: &str) {
    let entries: Vec<(String, String)> =
        std::env::vars().filter_map(|(key, value)| key.strip_prefix(prefix).map(|rest| (rest.to_ascii_lowercase(), value))).collect();
    for (path, value) in entries {
        let segments: Vec<&str> = path.split('.').collect();
        insert_path(base, &segments, TomlValue::String(value));
    }
}

fn insert_path(base: &mut TomlValue, segments: &[&str], value: TomlValue) {
    let Some((first, rest)) = segments.split_first()
    else {
        return;
    };
    let TomlValue::Object(table) = base
    else {
        return;
    };
    if rest.is_empty() {
        table.insert((*first).to_owned(), value);
        return;
    }
    let slot = table.entry((*first).to_owned()).or_insert_with(|| TomlValue::Object(TomlMap::new()));
    insert_path(slot, rest, value);
}
