use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    error::{CoreError, CoreResult},
    frontmatter::{FrontMatter, parse_document},
};

/// Stable content identity within a project (POSIX-ish relative path from content root).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContentId(pub String);

/// One discovery result: file identity, metadata and markdown body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentNode {
    /// Stable content identity within the project.
    pub id: ContentId,
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Relative path from `content_dir`.
    pub rel_path: String,
    /// File kind the node was discovered as.
    pub kind: ContentKind,
    /// Page metadata decoded from the front matter.
    pub front_matter: FrontMatterDto,
    /// Markdown body without front matter.
    pub body: String,
    /// Hex SHA-256 of the raw file bytes.
    pub content_hash: String,
}

/// Wire-friendly front matter subset (keeps full `fields` for plugins later).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontMatterDto {
    /// Page title used by templates and the head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional meta description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Page type (`home` / `post` / `page`) as written in the front matter.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub page_type: Option<String>,
    /// Explicit layout name overriding the type default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// Publication date as written in the front matter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// Draft pages are excluded from a production build.
    #[serde(default)]
    pub draft: bool,
    /// Sort weight within a page type; lower sorts first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<i64>,
    /// Taxonomy tags carried for aggregation and templates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Page locale overriding the site `lang`, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// Full front matter table for plugins and templates.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl From<FrontMatter> for FrontMatterDto {
    fn from(fm: FrontMatter) -> Self {
        Self {
            title: fm.title,
            description: fm.description,
            page_type: fm.page_type,
            layout: fm.layout,
            date: fm.date,
            draft: fm.draft,
            weight: fm.weight,
            tags: fm.tags,
            lang: fm.lang,
            fields: fm.fields,
        }
    }
}

/// How a discovered file participates in the content graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContentKind {
    /// Markdown source with optional front matter.
    Markdown,
    /// Any other discovered file (assets, data).
    Other,
}

/// Set of content nodes plus their dependency edges.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentGraph {
    /// Nodes keyed by content id.
    pub nodes: BTreeMap<ContentId, ContentNode>,
    /// id → dependency ids (templates/assets filled later).
    pub edges: BTreeMap<ContentId, BTreeSet<ContentId>>,
}

impl ContentGraph {
    /// Insert or replace the node keyed by its own id.
    pub fn insert(&mut self, node: ContentNode) {
        self.nodes.insert(node.id.clone(), node);
    }

    /// Remove a node and every edge that references it.
    pub fn remove(&mut self, id: &ContentId) -> Option<ContentNode> {
        self.edges.remove(id);
        for deps in self.edges.values_mut() {
            deps.remove(id);
        }
        self.nodes.remove(id)
    }

    /// Number of nodes in the graph.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the graph holds no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Dependents that list `id` as an edge target, plus `id` itself.
    pub fn invalidate_closure(&self, id: &ContentId) -> BTreeSet<ContentId> {
        let mut out = BTreeSet::new();
        out.insert(id.clone());
        for (from, tos) in &self.edges {
            if tos.contains(id) {
                out.insert(from.clone());
            }
        }
        out
    }
}

/// Scan `content_dir` and parse Markdown + front matter.
pub fn discover_markdown_content(project_root: &Path, content_dir: &str) -> CoreResult<ContentGraph> {
    let root = project_root.join(content_dir);
    let mut graph = ContentGraph::default();
    if !root.exists() {
        return Ok(graph);
    }
    walk_md(&root, &root, &mut graph)?;
    Ok(graph)
}

/// Load one markdown file into a content node, hashing the raw bytes.
pub fn load_markdown_node(content_root: &Path, abs_path: &Path) -> CoreResult<ContentNode> {
    let rel = abs_path.strip_prefix(content_root).unwrap_or(abs_path).to_string_lossy().replace('\\', "/");
    let raw = std::fs::read(abs_path).map_err(|source| CoreError::Io { source })?;
    let content_hash = Sha256::digest(&raw).iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let source = String::from_utf8_lossy(&raw);
    let doc = parse_document(&source)?;
    Ok(ContentNode {
        id: ContentId(rel.clone()),
        path: abs_path.to_path_buf(),
        rel_path: rel,
        kind: ContentKind::Markdown,
        front_matter: FrontMatterDto::from(doc.front_matter),
        body: doc.body,
        content_hash,
    })
}

fn walk_md(dir: &Path, content_root: &Path, graph: &mut ContentGraph) -> CoreResult<()> {
    let entries = std::fs::read_dir(dir).map_err(|source| CoreError::Io { source })?;
    for entry in entries {
        let entry = entry.map_err(|source| CoreError::Io { source })?;
        let path = entry.path();
        if path.is_dir() {
            walk_md(&path, content_root, graph)?;
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str())
        else {
            continue;
        };
        if !matches!(ext, "md" | "mdx" | "markdown") {
            continue;
        }
        let node = load_markdown_node(content_root, &path)?;
        graph.insert(node);
    }
    Ok(())
}
