//! Markdown 渲染与标题提取（文档语义归 `document.formats`）。

use std::path::Path;

/// 文档页面渲染上下文。
#[derive(Debug, Clone, Default)]
pub struct DocRenderContext {
    /// 当前 `.md` 文件相对分区根目录的路径（POSIX `/`）。
    pub relative_md_path: String,
}

impl DocRenderContext {
    /// 从相对 Markdown 路径构建上下文。
    pub fn from_relative_md(relative: impl Into<String>) -> Self {
        Self { relative_md_path: relative.into() }
    }

    fn current_dir(&self) -> String {
        Path::new(&self.relative_md_path).parent().map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default()
    }
}

/// HTML 渲染结果。
#[derive(Debug, Clone, Default)]
pub struct HtmlRenderResult {
    /// 正文 HTML。
    pub html: String,
    /// 是否包含公式（页面需公式样式）。
    pub has_katex: bool,
}

/// 将 Markdown 转为 HTML 正文片段。
pub fn render_markdown_to_html(source: &str, ctx: &DocRenderContext) -> HtmlRenderResult {
    let rewritten = rewrite_md_links(source, ctx);
    HtmlRenderResult { html: render_markdown_fragment(&rewritten), has_katex: false }
}

/// 将 Markdown 转为 HTML（无链接上下文，兼容旧调用）。
pub fn render_markdown_to_html_simple(source: &str) -> String {
    render_markdown_fragment(source)
}

/// 从 Markdown 提取第一个一级标题（ATX 优先，其次 setext）。
pub fn extract_title(source: &str) -> Option<String> {
    for line in source.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let title = rest.trim_end_matches('#').trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    let lines: Vec<&str> = source.lines().collect();
    for window in lines.windows(2) {
        let text = window[0].trim();
        let underline = window[1].trim();
        if !text.is_empty() && !underline.is_empty() && underline.chars().all(|ch| ch == '=') {
            return Some(text.to_string());
        }
    }
    None
}

fn render_markdown_fragment(source: &str) -> String {
    match document_formats::import::markdown::import_markdown_bytes("<legion-doc>", source) {
        Ok(graph) => document_formats::export::html::export_html(&graph).unwrap_or_default(),
        Err(_) => String::new(),
    }
}

fn rewrite_md_links(source: &str, ctx: &DocRenderContext) -> String {
    let _ = ctx.current_dir();
    source
        .lines()
        .map(|line| {
            if let Some((prefix, url)) = line.split_once("](") {
                if let Some(suffix) = url.strip_suffix(')') {
                    if suffix.ends_with(".md") {
                        let rewritten = format!("{}.html", &suffix[..suffix.len() - 3]);
                        return format!("{prefix}]({rewritten})");
                    }
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
