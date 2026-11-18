//! 缓存索引的 VON 书写器。
//!
//! 上游 serde 序列化器输出的是 `key=value` 方言，其解析器不接受该输出，因此这里按读取端
//! 能解析的 `key: value` 形状手写。

use std::collections::BTreeMap;

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 渲染 `{ packages: { "<name>@<version>": "<path>", … } }`。
pub(crate) fn render_package_index(packages: &BTreeMap<String, String>) -> String {
    let mut out = String::from("{\n");
    if !packages.is_empty() {
        out.push_str("    packages: {\n");
        for (key, path) in packages {
            out.push_str("        \"");
            out.push_str(&escape(key));
            out.push_str("\": \"");
            out.push_str(&escape(path));
            out.push_str("\",\n");
        }
        out.push_str("    },\n");
    }
    out.push_str("}\n");
    out
}
