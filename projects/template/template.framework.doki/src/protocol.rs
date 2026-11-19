//! Serializable segmented-build protocol types (N-API / Node plugin container).
//!
//! No Rust pointers, Dejavu objects, or builder handles.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{diagnostic::Diagnostic, graph::ContentId};

/// Requested build mode; `Check` validates without writing output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BuildMode {
    /// Development build: drafts included, unminified output.
    Development,
    /// Production build: drafts excluded, minified output.
    Production,
    /// Validation-only build that emits no files.
    Check,
}

/// Segment of the build pipeline currently holding the hand-off to Node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BuildPhase {
    /// Core finished discovery; may request `validateConfig` hooks.
    ValidateConfig,
    /// Ready for `transformContent` / `generateContent` batches.
    TransformContent,
    /// Core will render (Dejavu) — no plugin hooks in this stub phase yet.
    Render,
    /// Ready for `contributeArtifacts` / `buildComplete` batches after render.
    ContributeArtifacts,
    /// Ready for atomic commit.
    ReadyToCommit,
    /// Output committed.
    Committed,
}

/// Plugin hook the core is asking the Node side to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookKind {
    /// Validate the decoded config; may return diagnostics but no patch.
    ValidateConfig,
    /// Contribute additional content before graph closure.
    DiscoverContent,
    /// Rewrite markdown after front matter parsing.
    TransformContent,
    /// Generate virtual content pages from existing nodes.
    GenerateContent,
    /// Contribute template context values for rendering.
    ContributeContext,
    /// Rewrite a copied static asset.
    TransformAsset,
    /// Contribute client-side entry modules to the bundle.
    ContributeClientEntries,
    /// Contribute extra build artifacts after render.
    ContributeArtifacts,
    /// Observe the committed build and emit diagnostics only.
    BuildComplete,
}

/// One queued hook invocation, fully serialized across the N-API boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookWorkItem {
    /// Hook to invoke.
    pub hook: HookKind,
    /// Id of the plugin that owns the hook.
    pub plugin_id: String,
    /// Opaque JSON payload for the hook (domain snapshot, not internal objects).
    pub input: Value,
}

/// Result one plugin reports back for a hook invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginResult {
    /// Id of the plugin that produced this result.
    pub plugin_id: String,
    /// Hook the result answers.
    pub hook: HookKind,
    /// Whether the hook succeeded (defaults to `false`).
    #[serde(default)]
    pub ok: bool,
    /// Hook-specific JSON patch applied by the core, when the hook supports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<Value>,
    /// Diagnostics reported by the plugin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
    /// Files the plugin read; recorded as dependency edges for incremental builds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
}

/// One step of the segmented build: the phase reached and hooks still pending.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildPlan {
    /// Build instance id, stable across phases of the same build.
    pub build_id: String,
    /// Requested build mode.
    pub mode: BuildMode,
    /// Phase reached after the core's latest segment.
    pub phase: BuildPhase,
    /// Empty when the core can advance without Node.
    pub pending_hooks: Vec<HookWorkItem>,
    /// Number of content nodes known to the build at this point.
    pub content_count: usize,
    /// Diagnostics accumulated so far.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

/// Final result of a committed build.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildResult {
    /// Build instance id that produced this result.
    pub build_id: String,
    /// Output directory the build wrote to.
    pub output_dir: String,
    /// Content ids included in the build.
    pub content_ids: Vec<ContentId>,
    /// Files committed to the output directory, relative to it.
    pub committed_files: Vec<String>,
    /// Diagnostics accumulated over the whole build.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

/// One filesystem change reported by the host watcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    /// Project-relative path that changed.
    pub path: String,
    /// Kind of change observed.
    pub kind: FileChangeKind,
}

/// Kind of filesystem change observed by the watcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileChangeKind {
    /// File was created.
    Create,
    /// File contents changed.
    Modify,
    /// File was deleted.
    Remove,
}

/// Outcome of applying a set of [`FileChange`]s to the cached build state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvalidationResult {
    /// Content ids whose cached output is now stale.
    pub invalidated_content: Vec<ContentId>,
    /// Whether a rebuild is required at all.
    pub rebuild_required: bool,
}

/// Theme or plugin reference after Node-side resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedExtension {
    /// Stable extension id (package name or project-relative path).
    pub id: String,
    /// Whether this extension is a theme or a plugin.
    pub kind: ExtensionKind,
    /// How the extension was declared in `doki.toml`.
    pub source: ExtensionSourceKind,
    /// `path =` value from `doki.toml`, when declared that way.
    pub path: Option<String>,
    /// `package =` value from `doki.toml`, when declared that way.
    pub package: Option<String>,
    /// Absolute filesystem path after Node resolution (`path` or package root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<String>,
    /// Whether the extension is enabled.
    pub enabled: bool,
}

/// Explanation of why a build was (or was not) invalidated by a change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainRebuild {
    /// Path being explained.
    pub path: String,
    /// Whether this path forces a rebuild.
    pub rebuild_required: bool,
    /// Human-readable reason for the decision.
    pub reason: String,
    /// Content ids invalidated by this path.
    pub invalidated_content: Vec<ContentId>,
}

/// Role an extension plays in the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExtensionKind {
    /// Provides layouts, assets and client entries.
    Theme,
    /// Runs build hooks.
    Plugin,
}

/// Where an extension reference points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExtensionSourceKind {
    /// From `path =` in doki.toml (Node still resolves absolute path).
    LocalPath,
    /// From `package =` (registry or workspace — Node fills real path later).
    Package,
}
