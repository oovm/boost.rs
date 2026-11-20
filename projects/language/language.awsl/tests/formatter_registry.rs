//! 装配链验证：注册 provider 后，平台按扩展名 / 路径即可格式化，`.w` 与 `.awsl` 同源。

use std::path::PathBuf;

use language_format::formatter::{FormatOptions, format_path, format_source_for_extension, source_registry};

const SOURCE: &str = "<div   class = \"x\" ></div>";
const FORMATTED: &str = "<div class=\"x\"></div>\n";

fn unique_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vmz-formatter-awsl-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn extension_dispatch_covers_both_awsl_spellings() {
    language_awsl::register_source_formatter();

    let registry = source_registry().read().expect("registry");
    assert!(registry.supports_extension("awsl"));
    assert!(registry.supports_extension("w"));
    drop(registry);

    let options = FormatOptions::default();
    for extension in ["awsl", "w"] {
        let formatted = format_source_for_extension(extension, SOURCE, &options).expect("format .awsl/.w");
        assert_eq!(formatted, FORMATTED, "extension={extension}");
        assert_eq!(format_source_for_extension(extension, &formatted, &options).expect("idempotent"), formatted);
    }
}

#[test]
fn format_path_rewrites_a_short_awsl_file() {
    language_awsl::register_source_formatter();

    let dir = unique_dir("path");
    let path = dir.join("sample.w");
    std::fs::write(&path, SOURCE).expect("write sample");

    let outcome = format_path(&path, &FormatOptions::default()).expect("format path");
    assert!(outcome.changed);
    assert_eq!(outcome.formatted, FORMATTED);

    std::fs::remove_dir_all(&dir).ok();
}
