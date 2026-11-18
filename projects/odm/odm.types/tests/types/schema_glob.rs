//! 多文件 schema 路径收集：单一文件、目录与 glob 模式三条入口。

use boost_fs::{temp::create_folder_in, write};
use odm_types::collect_schema_paths;

fn folder() -> boost_fs::temp::TempFolder {
    create_folder_in(std::env::temp_dir()).expect("temporary folder")
}

#[test]
fn glob_pattern_walks_nested_schema_folders() {
    let root = folder();
    let base = root.path().join("schemas/nested");
    std::fs::create_dir_all(&base).expect("schema folders");
    write(root.path().join("schemas/user.iris"), b"schema").expect("schema file");
    write(base.join("order.iris"), b"schema").expect("schema file");
    write(base.join("note.txt"), b"not a schema").expect("text file");

    let paths = collect_schema_paths(root.path(), "schemas/**/*.iris").expect("glob matches");
    let names: Vec<String> = paths.iter().map(|path| path.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["order.iris", "user.iris"]);
}

#[test]
fn non_schema_files_are_not_collected() {
    let root = folder();
    std::fs::create_dir_all(root.path().join("schemas")).expect("schema folder");
    write(root.path().join("schemas/readme.md"), b"# schema").expect("markdown file");

    let error = collect_schema_paths(root.path(), "schemas/*.md").unwrap_err();
    assert!(error.contains("no `.iris` files matched"), "{error}");
}
