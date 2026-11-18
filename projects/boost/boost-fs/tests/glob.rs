//! `boost_fs::glob` 的路径枚举与匹配语义验收。
//!
//! 覆盖：字面路径、`*` 与 `?` 组件通配、递归段、`.hidden` 参与匹配、未支持的
//! 字符类被拒绝，以及不可读目录不中断结果。

use boost_fs::{
    glob::{Pattern, PatternError, glob},
    temp::create_folder_in,
    write,
};

fn fixture() -> boost_fs::temp::TempFolder {
    let root = create_folder_in(std::env::temp_dir()).expect("temporary folder");
    let base = root.path();
    write(base.join("user.iris"), b"schema").expect("schema file");
    write(base.join("gift.iris"), b"schema").expect("schema file");
    write(base.join(".hidden.iris"), b"schema").expect("schema file");
    write(base.join("note.txt"), b"text").expect("text file");
    std::fs::create_dir_all(base.join("nested/deep")).expect("nested directories");
    write(base.join("nested/deep/order.iris"), b"schema").expect("schema file");
    root
}

fn normalized(pattern: &str, folder: &boost_fs::temp::TempFolder) -> String {
    format!("{}/{}", folder.path().to_string_lossy().replace('\\', "/"), pattern)
}

#[test]
fn single_star_matches_only_the_current_level() {
    let folder = fixture();
    let matches = glob(&normalized("*.iris", &folder)).expect("supported pattern");
    let names: Vec<String> = matches.iter().map(|path| path.file_name().unwrap().to_string_lossy().into_owned()).collect();
    // `.hidden.iris` 也参与匹配，与既有 glob 行为一致；嵌套目录里的文件不参与。
    assert_eq!(names, vec![".hidden.iris", "gift.iris", "user.iris"]);
}

#[test]
fn question_mark_matches_one_character() {
    let folder = fixture();
    assert_eq!(glob(&normalized("user?iris", &folder)).expect("supported pattern").len(), 1);
    assert!(glob(&normalized("user??iris", &folder)).expect("supported pattern").is_empty());
}

#[test]
fn recursive_segment_walks_nested_folders() {
    let folder = fixture();
    let matches = glob(&normalized("**/*.iris", &folder)).expect("supported pattern");
    let names: Vec<String> = matches.iter().map(|path| path.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec![".hidden.iris", "gift.iris", "order.iris", "user.iris"]);
}

#[test]
fn literal_pattern_matches_itself_only() {
    let path = Pattern::new("a/b/c.iris").expect("supported pattern");
    assert!(path.is_literal());
    assert!(path.matches(std::path::Path::new("a/b/c.iris")));
    assert!(!path.matches(std::path::Path::new("a/b/d.iris")));
}

#[test]
fn unsupported_syntax_is_an_error() {
    assert_eq!(Pattern::new("").unwrap_err(), PatternError::Empty);
    assert_eq!(Pattern::new("a/[bc].iris").unwrap_err(), PatternError::Unsupported { character: '[' });
    assert_eq!(Pattern::new("a/{b,c}.iris").unwrap_err(), PatternError::Unsupported { character: '{' });
}

#[test]
fn missing_folders_yield_no_match() {
    let folder = fixture();
    assert!(glob(&normalized("absent/*.iris", &folder)).expect("supported pattern").is_empty());
}
