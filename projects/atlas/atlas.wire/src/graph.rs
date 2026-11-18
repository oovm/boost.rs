//! 冻结图的静态验证：缺失、环与作用域。

use std::collections::{BTreeMap, BTreeSet};

use boost_collection::GraphView;
use boost_math::graph_theory::cycle_path;

use crate::{
    dependency::Dependency,
    error::{WireError, WireErrorKind},
    key::TypeKey,
    provider::Entry,
    scope::Scope,
};

/// 校验整张冻结图。
pub(crate) fn validate_graph(entries: &BTreeMap<TypeKey, Entry>) -> Result<(), WireError> {
    for entry in entries.values() {
        let Entry::Provider(provider) = entry
        else {
            continue;
        };
        for dependency in &provider.dependencies {
            if dependency.optional() {
                continue;
            }
            if !entries.contains_key(&dependency.key()) {
                return Err(WireError::new(
                    WireErrorKind::NotRegistered,
                    format!("provider `{}` 的依赖 `{}` 未注册", provider.label, dependency.label()),
                )
                .with_path(&[provider.label]));
            }
        }
    }
    detect_cycles(entries)?;
    for entry in entries.values() {
        let Entry::Provider(provider) = entry
        else {
            continue;
        };
        if provider.scope != Scope::Application {
            continue;
        }
        let mut visited = BTreeSet::new();
        let mut path = vec![provider.label];
        reach_request(entries, &provider.dependencies, &mut visited, &mut path)?;
    }
    Ok(())
}

/// 校验根组件的直接依赖：缺失、环与应用级作用域。
pub(crate) fn validate_root_dependencies(entries: &BTreeMap<TypeKey, Entry>, dependencies: &[Dependency]) -> Result<(), WireError> {
    for dependency in dependencies {
        if dependency.optional() {
            continue;
        }
        if !entries.contains_key(&dependency.key()) {
            return Err(WireError::new(WireErrorKind::NotRegistered, format!("根组件的依赖 `{}` 未注册", dependency.label())));
        }
    }
    let mut visited = BTreeSet::new();
    let mut path = Vec::new();
    reach_request(entries, dependencies, &mut visited, &mut path)
}

fn detect_cycles(entries: &BTreeMap<TypeKey, Entry>) -> Result<(), WireError> {
    // 普通有向静态图：顶点是注册键，边是 provider 到已注册依赖的引用。
    // 环检测本身复用共享图论核，本模块只负责把环映射回标签诊断。
    let mut view = GraphView::new(true);
    for (key, entry) in entries {
        view.add_vertex(key);
        let Entry::Provider(provider) = entry
        else {
            continue;
        };
        for dependency in &provider.dependencies {
            if let Some((target, _)) = entries.get_key_value(&dependency.key()) {
                view.add_edge(key, target);
            }
        }
    }

    let Some(cycle) = cycle_path(&view).ok().flatten()
    else {
        return Ok(());
    };
    let labels: Vec<&'static str> = cycle.iter().map(|key| label_of(entries, *key)).collect();
    Err(WireError::new(WireErrorKind::Cycle, "依赖图存在环").with_path(&labels))
}

fn label_of(entries: &BTreeMap<TypeKey, Entry>, key: &TypeKey) -> &'static str {
    match entries.get(key) {
        Some(Entry::Provider(provider)) => provider.label,
        Some(Entry::Slot(slot)) => slot.label,
        None => "<unregistered>",
    }
}

/// 从应用级作用域出发，确认依赖闭包（含经 transient 的间接路径）不会触达请求级键。
fn reach_request(
    entries: &BTreeMap<TypeKey, Entry>,
    dependencies: &[Dependency],
    visited: &mut BTreeSet<TypeKey>,
    path: &mut Vec<&'static str>,
) -> Result<(), WireError> {
    for dependency in dependencies {
        let Some(entry) = entries.get(&dependency.key())
        else {
            continue;
        };
        match entry {
            Entry::Slot(slot) => {
                return Err(
                    WireError::new(WireErrorKind::ScopeViolation, format!("应用级依赖触达请求上下文槽位 `{}`", slot.label)).with_path(path)
                );
            }
            Entry::Provider(provider) => {
                if provider.scope == Scope::Request {
                    return Err(WireError::new(WireErrorKind::ScopeViolation, format!("应用级依赖触达请求级 provider `{}`", provider.label))
                        .with_path(path));
                }
                if !visited.insert(dependency.key()) {
                    continue;
                }
                path.push(provider.label);
                reach_request(entries, &provider.dependencies, visited, path)?;
                path.pop();
            }
        }
    }
    Ok(())
}
