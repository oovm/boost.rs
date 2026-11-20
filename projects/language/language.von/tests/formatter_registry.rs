//! 装配链验证：注册 provider 后，平台按扩展名 / 路径即可格式化，且结果幂等。

use std::path::PathBuf;

use language_format::formatter::{FormatOptions, format_path, format_source_for_extension, source_registry};

fn unique_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vmz-formatter-von-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn extension_dispatch_reaches_the_von_formatter() {
    language_von::register_source_formatter();

    assert!(source_registry().read().expect("registry").supports_extension("von"));

    let options = FormatOptions::default();
    let formatted = format_source_for_extension("von", "{a:1}", &options).expect("format .von");
    assert_eq!(formatted, "{ a: 1 }\n");
    assert_eq!(format_source_for_extension("von", &formatted, &options).expect("idempotent"), formatted);
}

#[test]
fn format_path_rewrites_a_von_file() {
    language_von::register_source_formatter();

    let dir = unique_dir("path");
    let path = dir.join("sample.von");
    std::fs::write(&path, "{a:1}").expect("write sample");

    let outcome = format_path(&path, &FormatOptions::default()).expect("format path");
    assert!(outcome.changed);
    assert_eq!(outcome.formatted, "{ a: 1 }\n");

    std::fs::remove_dir_all(&dir).ok();
}
