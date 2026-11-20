//! AWSL 静态渲染代码生成：Render IR → 静态 HTML。

pub mod expr_eval;
pub mod html_render;

pub use html_render::{StaticRenderResult, render_static_ir};
