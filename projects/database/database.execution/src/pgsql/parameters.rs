//! PostgreSQL GUC surface the gateway reports.
//!
//! The wire session answers `SHOW <parameter>` and the SQL function
//! `current_setting(name)` from the same table, so the two cannot drift. Only
//! `search_path` and `statement_timeout` are session-mutable; every other
//! parameter reports its PostgreSQL default.

use super::session::DEFAULT_SEARCH_PATH;

/// Parameters the gateway answers with their default value.
///
/// `server_version` is deliberately absent: the server crate owns the banner and
/// resolves that name through its own version function.
pub const PG_PARAMETERS: &[(&str, &str)] = &[
    ("application_name", ""),
    ("client_encoding", "UTF8"),
    ("client_min_messages", "notice"),
    ("datestyle", "ISO, MDY"),
    ("default_transaction_isolation", "read committed"),
    ("extra_float_digits", "3"),
    ("integer_datetimes", "on"),
    ("intervalstyle", "postgres"),
    ("server_encoding", "UTF8"),
    ("search_path", DEFAULT_SEARCH_PATH),
    ("statement_timeout", "0"),
    ("standard_conforming_strings", "on"),
    ("timezone", "UTC"),
    ("transaction_isolation", "read committed"),
    ("transaction_read_only", "off"),
    ("default_transaction_read_only", "off"),
];

/// Case-insensitive lookup of a parameter's default value.
pub fn default_parameter(name: &str) -> Option<&'static str> {
    let needle = name.trim().to_ascii_lowercase();
    PG_PARAMETERS.iter().find(|(key, _)| *key == needle).map(|(_, value)| *value)
}
