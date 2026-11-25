//! 文件级条件筛选的 planner 编排：读取快照文件并调用语言侧条件判定。
//!
//! 前导 AST 解析与 `[when]`/`[gate]` 求值语义归 `language_valkyrie::compiler::syntax::conditions`
//! 所有；本模块只做 IO、错误映射与语义组组装，不再自行解析 Valkyrie 文件前导 AST。

use std::{collections::BTreeSet, fs, path::PathBuf};

use compiler_target::CanonicalTarget;
use language_valkyrie::compiler::syntax::conditions::source_file_conditions_pass;

use super::types::{PlannedSemanticSourceGroup, PlannerError, PlannerResult};

pub(crate) fn filter_semantic_source_groups_for_target(
    groups: Vec<PlannedSemanticSourceGroup>,
    target: CanonicalTarget,
    enabled_features: Option<&BTreeSet<String>>,
) -> PlannerResult<Vec<PlannedSemanticSourceGroup>> {
    groups
        .into_iter()
        .map(|mut group| {
            group.source_files = filter_paths_for_target(group.source_files, target, enabled_features)?;
            Ok(group)
        })
        .collect()
}

pub(crate) fn filter_source_files_for_target(
    paths: Vec<PathBuf>,
    target: CanonicalTarget,
    enabled_features: Option<&BTreeSet<String>>,
) -> PlannerResult<Vec<PathBuf>> {
    filter_paths_for_target(paths, target, enabled_features)
}

fn filter_paths_for_target(
    paths: Vec<PathBuf>,
    target: CanonicalTarget,
    enabled_features: Option<&BTreeSet<String>>,
) -> PlannerResult<Vec<PathBuf>> {
    let mut filtered = Vec::with_capacity(paths.len());
    for path in paths {
        let content = fs::read_to_string(&path).map_err(PlannerError::Io)?;
        let source_id = path.to_string_lossy().replace('\\', "/");
        let active = source_file_conditions_pass(&content, target, enabled_features, &source_id)
            .map_err(|message| PlannerError::UnparsableSource { path: path.clone(), message })?;
        if active {
            filtered.push(path);
        }
    }
    Ok(filtered)
}

#[cfg(test)]
mod tests {
    use language_valkyrie::compiler::syntax::conditions::source_file_active_for_target;

    use compiler_target::CanonicalTarget;
    use std::path::PathBuf;

    /// 真实 std 适配器语料回归：文件级 `when` 必须按目标区分 browser / wechat 专属实现。
    #[test]
    fn valkyrie_corpus_fetch_when_files_track_target() {
        let corpus =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../../valkyrie.v/projects/std.adaptors._/std.adaptor.wasm/source");
        if !corpus.is_dir() {
            eprintln!("skip: valkyrie.v adaptor source not beside nyar-vm.rs");
            return;
        }
        let shared = std::fs::read_to_string(corpus.join("fetch.v")).expect("fetch.v");
        assert!(source_file_active_for_target(&shared, CanonicalTarget::browser()));
        let wechat_target = CanonicalTarget::wechat();
        assert!(source_file_active_for_target(&shared, wechat_target));

        let browser = std::fs::read_to_string(corpus.join("fetch.browser.v")).expect("fetch.browser.v");
        assert!(source_file_active_for_target(&browser, CanonicalTarget::browser()));
        assert!(!source_file_active_for_target(&browser, wechat_target));

        let wechat = std::fs::read_to_string(corpus.join("fetch.wechat.v")).expect("fetch.wechat.v");
        assert!(!source_file_active_for_target(&wechat, CanonicalTarget::browser()));
        assert!(source_file_active_for_target(&wechat, wechat_target));
    }
}
