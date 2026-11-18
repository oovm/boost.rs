//! 文档站点资产：由宿主显式提供，Boost 不内嵌产品模板与样式。

use std::{
    fs,
    path::{Path, PathBuf},
};

/// 生成文档站点所需的四份资产。
///
/// 资产是产品夹具，按 Boost 的测试/夹具边界留在下游；本模块只从显式目录读取，
/// 不 `include_str!` 任何产品文件，也不提供内嵌兜底。
#[derive(Debug, Clone)]
pub struct DocAssets {
    /// 资产所在目录。
    pub directory: PathBuf,
    /// 基础样式。
    pub base_css: String,
    /// 页面布局模板。
    pub layout_awsl: String,
    /// 文档页模板。
    pub doc_page_awsl: String,
    /// hub 页模板。
    pub hub_awsl: String,
}

impl DocAssets {
    /// 相对项目根的默认资产目录。
    pub const DIRECTORY: &'static str = "assets/doc";

    /// `root/assets/doc`。
    pub fn default_directory(root: impl AsRef<Path>) -> PathBuf {
        root.as_ref().join(Self::DIRECTORY)
    }

    /// 从显式目录读取四份资产；缺任何一份即闭合失败，不回退内嵌拷贝。
    pub fn load(directory: impl AsRef<Path>) -> std::io::Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        Ok(Self {
            base_css: fs::read_to_string(directory.join("base.css"))?,
            layout_awsl: fs::read_to_string(directory.join("layout.awsl"))?,
            doc_page_awsl: fs::read_to_string(directory.join("doc-page.awsl"))?,
            hub_awsl: fs::read_to_string(directory.join("hub.awsl"))?,
            directory,
        })
    }
}

/// 写出 `legion-document.css`。
pub fn write_document_css(output_dir: &Path, base_css: &str, component_css: &str) -> std::io::Result<()> {
    fs::create_dir_all(output_dir)?;
    let mut css = String::new();
    css.push_str(base_css);
    css.push('\n');
    if !component_css.trim().is_empty() {
        css.push_str("/* component styles */\n");
        css.push_str(component_css);
        css.push('\n');
    }
    fs::write(output_dir.join("legion-document.css"), css)
}
