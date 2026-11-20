//! Portable archive entry path validation shared by container codecs.
//!
//! This module owns the shape checks that hold for every archive format and
//! every host: separator unification, component decomposition, parent-traversal
//! escape, absoluteness and the reserved device names that outlive any
//! normalization. Host policy (drive letters, UNC prefixes, trailing dot or
//! space normalization and case folding) stays with the caller, because only
//! the caller knows which filesystems the extraction target may touch.

use std::fmt;

/// A validated portable entry path: non-empty relative components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortablePath {
    /// Path components in order, with empty and `.` segments removed.
    pub components: Vec<String>,
}

impl PortablePath {
    /// The components joined with forward slashes.
    pub fn display(&self) -> String {
        self.components.join("/")
    }
}

/// Portable path violations shared by all container codecs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathViolation {
    /// The stored name is empty.
    Empty,
    /// The name contains NUL or another control character (tabs are allowed).
    ControlCharacter,
    /// The name starts with a separator and is therefore absolute.
    Absolute,
    /// The name contains a `..` segment that could escape the destination.
    ParentTraversal,
    /// A component is a reserved device name on Windows-family hosts.
    ReservedDeviceName {
        /// The offending component.
        component: String,
    },
    /// Every segment collapsed to nothing, so the name resolves to the root.
    ResolvesToRoot,
}

impl fmt::Display for PathViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "archive entry has an empty name"),
            Self::ControlCharacter => write!(f, "archive entry contains control characters"),
            Self::Absolute => write!(f, "archive entry is an absolute path"),
            Self::ParentTraversal => write!(f, "archive entry contains a parent traversal segment"),
            Self::ReservedDeviceName { component } => write!(f, "archive entry uses the reserved device name `{component}`"),
            Self::ResolvesToRoot => write!(f, "archive entry resolves to the destination root"),
        }
    }
}

impl std::error::Error for PathViolation {}

/// Validates one stored entry name against the portable path rules.
///
/// Backslashes are treated as separators because writers from Windows hosts
/// store them verbatim, which would otherwise smuggle `..\evil.txt` past a
/// slash-only check. `..` segments are rejected outright instead of resolved,
/// so no destination-relative join can escape. `reject_device_names` enables
/// the reserved device name check for targets that may land on Windows-family
/// filesystems; it is a portable rule (the names never change) that callers
/// gate on their platform policy.
pub fn validate_portable_path(name: &str, reject_device_names: bool) -> Result<PortablePath, PathViolation> {
    if name.is_empty() {
        return Err(PathViolation::Empty);
    }
    if name.chars().any(|character| character == '\0' || (character.is_control() && character != '\t')) {
        return Err(PathViolation::ControlCharacter);
    }

    let unified = name.replace('\\', "/");
    if unified.starts_with('/') {
        return Err(PathViolation::Absolute);
    }

    let mut components = Vec::new();
    for component in unified.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err(PathViolation::ParentTraversal);
        }
        if reject_device_names && is_reserved_device_name(component) {
            return Err(PathViolation::ReservedDeviceName { component: component.to_string() });
        }
        components.push(component.to_string());
    }

    if components.is_empty() {
        return Err(PathViolation::ResolvesToRoot);
    }
    Ok(PortablePath { components })
}

/// Whether a path component is a reserved Windows device name.
///
/// The base name before the first dot decides, so `CON.txt` and `com1.log`
/// are reserved, and `COM0`/`LPT0` are not (only 1 through 9 are).
pub fn is_reserved_device_name(component: &str) -> bool {
    let base = component.split('.').next().unwrap_or(component).trim().to_ascii_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$")
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.as_bytes()[3].is_ascii_digit()
            && base.as_bytes()[3] != b'0')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reject(name: &str) -> PathViolation {
        validate_portable_path(name, true).expect_err("must reject")
    }

    #[test]
    fn plain_nested_paths_are_accepted() {
        let path = validate_portable_path("docs/notes/report.txt", true).expect("accepted");
        assert_eq!(path.display(), "docs/notes/report.txt");
        assert_eq!(path.components, vec!["docs".to_string(), "notes".to_string(), "report.txt".to_string()]);
    }

    #[test]
    fn traversal_and_absolute_shapes_are_rejected() {
        assert_eq!(reject("../evil.txt"), PathViolation::ParentTraversal);
        assert_eq!(reject("docs/../../evil.txt"), PathViolation::ParentTraversal);
        assert_eq!(reject("..\\evil.txt"), PathViolation::ParentTraversal);
        assert_eq!(reject("/absolute/evil.txt"), PathViolation::Absolute);
        assert_eq!(reject("\\\\server\\share\\evil.txt"), PathViolation::Absolute);
    }

    #[test]
    fn dot_segments_collapse_but_never_escape() {
        let path = validate_portable_path("docs/./notes/report.txt", true).expect("accepted");
        assert_eq!(path.display(), "docs/notes/report.txt");
        assert_eq!(reject("./"), PathViolation::ResolvesToRoot);
        assert_eq!(reject("././"), PathViolation::ResolvesToRoot);
        assert_eq!(reject("///"), PathViolation::Absolute);
    }

    #[test]
    fn control_characters_and_empty_names_are_rejected() {
        assert_eq!(reject(""), PathViolation::Empty);
        assert_eq!(reject("bad\0name"), PathViolation::ControlCharacter);
        assert_eq!(reject("bad\nname"), PathViolation::ControlCharacter);
        assert!(validate_portable_path("tab\there.txt", true).is_ok(), "tabs stay legal");
    }

    #[test]
    fn device_names_follow_the_gate() {
        assert_eq!(reject("CON"), PathViolation::ReservedDeviceName { component: "CON".to_string() });
        assert_eq!(reject("con.txt"), PathViolation::ReservedDeviceName { component: "con.txt".to_string() });
        assert_eq!(reject("COM1/evil.txt"), PathViolation::ReservedDeviceName { component: "COM1".to_string() });
        assert!(validate_portable_path("CON", false).is_ok());
        assert!(validate_portable_path("COM0.txt", true).is_ok(), "COM0 is not reserved");
        assert!(validate_portable_path("CONFIG.sys", true).is_ok(), "prefix matches are not reserved");
    }
}
