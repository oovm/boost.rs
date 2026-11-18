//! 冻结图的静态验证：缺失、环与作用域。

use std::collections::{BTreeMap, BTreeSet, HashMap};

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    Gray,
    Black,
}

fn detect_cycles(entries: &BTreeMap<TypeKey, Entry>) -> Result<(), WireError> {
    let mut colors: HashMap<TypeKey, Color> = HashMap::new();
    let mut path = Vec::new();
    for key in entries.keys().copied() {
        visit(key, entries, &mut colors, &mut path)?;
    }
    Ok(())
}

fn visit(
    key: TypeKey,
    entries: &BTreeMap<TypeKey, Entry>,
    colors: &mut HashMap<TypeKey, Color>,
    path: &mut Vec<&'static str>,
) -> Result<(), WireError> {
    match colors.get(&key).copied() {
        Some(Color::Black) => return Ok(()),
        Some(Color::Gray) => {
            return Err(WireError::new(WireErrorKind::Cycle, "依赖图存在环").with_path(path));
        }
        None => {}
    }
    colors.insert(key, Color::Gray);
    if let Some(Entry::Provider(provider)) = entries.get(&key) {
        path.push(provider.label);
        for dependency in &provider.dependencies {
            if entries.contains_key(&dependency.key()) {
                visit(dependency.key(), entries, colors, path)?;
            }
        }
        path.pop();
    }
    colors.insert(key, Color::Black);
    Ok(())
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
