//! 宿主字体包解析。
//!
//! PDF 写出需要宿主自备正文 face 包；Boost 不携带字体二进制。下游按两种入口取包：
//!
//! - [`host_font_dir`]：环境变量 `VUTEX_FONT_DIR` 优先，其次从当前目录向上找
//!   `VuTeX/projects/vutex.rs/fonts`（VuTeX 与消费仓同盘并列时命中）。
//! - [`pdf_body_faces`]：解析成功返回 face 集合，失败返回 `None`，供可跳过的用例使用。
//! - [`require_pdf_body_faces`]：缺包直接失败，供必须覆盖 PDF 写出合同的用例使用。

use std::{path::PathBuf, sync::Arc};

use document_fonts::BodyFaceSet;

/// 解析宿主字体包目录；找不到时返回 `None`。
pub fn host_font_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("VUTEX_FONT_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            return Some(dir);
        }
    }
    let mut current = std::env::current_dir().ok()?;
    loop {
        let candidate = current.join("VuTeX").join("projects").join("vutex.rs").join("fonts");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// 加载宿主字体包的正文 face；缺包时返回 `None`。
pub fn pdf_body_faces() -> Option<Arc<BodyFaceSet>> {
    let dir = host_font_dir()?;
    Some(Arc::new(BodyFaceSet::load_from(&dir).unwrap_or_else(|error| panic!("read body faces from {}: {error}", dir.display()))))
}

/// 加载宿主字体包的正文 face；缺包直接失败。
pub fn require_pdf_body_faces() -> Arc<BodyFaceSet> {
    let dir = host_font_dir().expect("PDF tests need the host font pack; set VUTEX_FONT_DIR or check out VuTeX beside the consuming repo");
    Arc::new(BodyFaceSet::load_from(&dir).unwrap_or_else(|error| panic!("read body faces from {}: {error}", dir.display())))
}
