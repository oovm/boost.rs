//! Checking that an exported work still carries every file it references.
//!
//! An archive is only reopenable when the production's asset closure matches
//! what is on disk. This module re-reads each referenced file and compares its
//! digest with the value recorded when the asset entered the work, so a moved,
//! truncated or silently replaced file is reported instead of being packaged.
//!
//! Only the asset closure is checked. Rendering, encoding and packaging stay in
//! their own owners; how the files are zipped is not decided here.

use std::path::{Component, Path};

use media_scene::Production;
use media_types::Error;
use serde::{Deserialize, Serialize};

/// One asset as found relative to the archive root.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AssetCheck {
    /// Asset handle.
    pub id: String,
    /// Path recorded in the production.
    pub path: String,
    /// Recorded digest.
    pub digest: String,
    /// Digest of the file on disk, when it could be read.
    pub found: Option<String>,
}

/// Outcome of checking a production against a directory tree.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ArchiveReport {
    /// Root the assets were resolved against.
    pub root: String,
    /// Assets that matched their recorded digest.
    pub ok: Vec<AssetCheck>,
    /// Assets whose file is absent.
    pub missing: Vec<AssetCheck>,
    /// Assets whose file no longer matches the recorded digest.
    pub mismatched: Vec<AssetCheck>,
}

impl ArchiveReport {
    /// Whether the closure is complete and unchanged.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty() && self.mismatched.is_empty()
    }
}

/// Re-reads every asset of `production` under `root` and reports the closure.
///
/// A production that fails its own validation is an error: checking a file list
/// that already contradicts itself would only produce noise.
pub fn check_archive(root: &Path, production: &Production) -> Result<ArchiveReport, Error> {
    production.validate()?;

    let mut report = ArchiveReport {
        root: root.to_string_lossy().into_owned(),
        ok: Vec::new(),
        missing: Vec::new(),
        mismatched: Vec::new(),
    };

    for asset in &production.assets {
        let check = AssetCheck { id: asset.id.to_string(), path: asset.path.clone(), digest: asset.digest.clone(), found: None };
        // The recorded path is already constrained to stay inside the root, but
        // an archive check is exactly where a second look belongs.
        if escapes(&asset.path) {
            let mut check = check;
            check.found = Some("path leaves the archive root".into());
            report.mismatched.push(check);
            continue;
        }
        let file = root.join(asset.path.replace('\\', "/"));
        let Ok(bytes) = std::fs::read(&file) else {
            report.missing.push(check);
            continue;
        };
        let digest = media_types::digest(&bytes);
        if digest == asset.digest {
            report.ok.push(AssetCheck { found: Some(digest), ..check });
        } else {
            report.mismatched.push(AssetCheck { found: Some(digest), ..check });
        }
    }

    Ok(report)
}

/// Whether a relative path uses an absolute prefix, a root, or a `..` step.
fn escapes(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    Path::new(&normalized).components().any(|component| matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn production(path: &str, digest: &str) -> Production {
        let json = serde_json::json!({
            "project": {
                "width": 160, "height": 90, "fps": 15, "duration_frames": 30,
                "scenes": [{ "start": 0, "duration": 30,
                    "background": { "kind": "solid", "color": "#203040" }, "items": [] }],
                "audio": null
            },
            "assets": [{
                "id": "a1", "path": path, "kind": "image", "digest": digest, "source": { "kind": "file" }
            }]
        });
        serde_json::from_value(json).expect("production")
    }

    fn workspace(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("opencut-archive-tests").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_unchanged_asset_is_accepted() {
        let dir = workspace("intact");
        std::fs::write(dir.join("cover.png"), b"pixels").unwrap();
        let digest = media_types::digest(b"pixels");
        let report = check_archive(&dir, &production("cover.png", &digest)).expect("check");
        assert!(report.is_complete());
        assert_eq!(report.ok.len(), 1);
        assert_eq!(report.ok[0].found.as_deref(), Some(digest.as_str()));
    }

    #[test]
    fn a_changed_asset_is_reported_with_both_digests() {
        let dir = workspace("changed");
        std::fs::write(dir.join("cover.png"), b"other").unwrap();
        let recorded = media_types::digest(b"pixels");
        let report = check_archive(&dir, &production("cover.png", &recorded)).expect("check");
        assert!(!report.is_complete());
        assert_eq!(report.mismatched.len(), 1);
        assert_eq!(report.mismatched[0].digest, recorded);
        assert_eq!(report.mismatched[0].found.as_deref(), Some(media_types::digest(b"other").as_str()));
    }

    #[test]
    fn a_missing_asset_is_reported_instead_of_packaging_a_hole() {
        let dir = workspace("missing");
        let digest = media_types::digest(b"pixels");
        let report = check_archive(&dir, &production("gone.png", &digest)).expect("check");
        assert!(!report.is_complete());
        assert_eq!(report.missing.len(), 1);
        assert!(report.missing[0].found.is_none());
    }

    #[test]
    fn a_path_that_escapes_the_root_is_never_read() {
        // The production itself rejects such a path at construction; the check
        // repeats the rule so a future model change cannot quietly weaken it.
        assert!(escapes("../outside.png"));
        assert!(escapes("C:/Windows/system.ini"));
        assert!(!escapes("assets/cover.png"));
    }
}
