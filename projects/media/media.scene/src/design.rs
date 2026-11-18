//! Creative design tokens: semantic colors resolved by explicit layers.
//!
//! Authors write `$role` wherever a color string is accepted and declare the
//! role once in a [`ColorCard`]. Resolution is deterministic and reported:
//! the project card is the base, a scene card may override it unless the
//! project entry is locked, and every rewritten field is recorded with its
//! scene, node and path. The renderer only ever sees concrete colors, so no
//! paint path needs to know about tokens.
//!
//! Locking is a contract, not a UI hint: a scene override of a locked role is
//! an explicit `invalid_scene` error naming the role and the scene.

use std::{collections::HashMap, fmt::Write};

use media_types::Error;
use serde::{Deserialize, Serialize};

use crate::scene::{Background, Item, Node, NodeId, Project, Scene};

/// Token prefix in every color string, e.g. `"$accent"`.
pub const TOKEN_PREFIX: char = '$';

/// One semantic color.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ColorEntry {
    /// Stable role name, e.g. `accent` or `ink-muted`.
    pub role: String,
    /// Concrete color value (`#rgb`, `#rrggbb` or `#rrggbbaa`).
    pub value: String,
    /// A locked role cannot be overridden by a scene card.
    #[serde(default)]
    pub locked: bool,
}

/// Design data declared on a project or a scene.
///
/// The same shape is used at both layers: the project card is the base and a
/// scene card is an override set, so there is one declaration format and one
/// resolution rule.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct DesignSpec {
    /// Semantic colors declared at this layer.
    #[serde(default)]
    pub colors: Vec<ColorEntry>,
}

impl DesignSpec {
    /// Looks up one declared role.
    #[must_use]
    pub fn get(&self, role: &str) -> Option<&ColorEntry> {
        self.colors.iter().find(|entry| entry.role == role)
    }
}

/// Where a resolved color came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorSource {
    /// Declared on the project card.
    Project,
    /// Declared or overridden on the scene card.
    Scene,
}

impl ColorSource {
    /// Lowercase label used in reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Scene => "scene",
        }
    }
}

/// One rewritten color reference.
#[derive(Debug, Clone, PartialEq)]
pub struct ColorRef {
    /// Semantic role that was referenced.
    pub role: String,
    /// Concrete value written into the field.
    pub value: String,
    /// Layer the value came from.
    pub source: ColorSource,
    /// Scene index, or `None` for reusable component definitions.
    pub scene: Option<usize>,
    /// Node identity when the reference sits on a node that declares one.
    pub node: Option<NodeId>,
    /// Structural path, e.g. `scenes[1].items[2].color`.
    pub path: String,
}

/// Impact report for one resolution pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DesignReport {
    /// Every rewritten color reference, in traversal order.
    pub colors: Vec<ColorRef>,
}

impl DesignReport {
    /// References that use `role`.
    pub fn affected(&self, role: &str) -> Vec<&ColorRef> {
        self.colors.iter().filter(|entry| entry.role == role).collect()
    }

    /// Scene indices that reference `role`, ascending and deduplicated.
    pub fn affected_scenes(&self, role: &str) -> Vec<usize> {
        let mut scenes: Vec<usize> = self.colors.iter().filter(|entry| entry.role == role).filter_map(|entry| entry.scene).collect();
        scenes.sort_unstable();
        scenes.dedup();
        scenes
    }

    /// Distinct referenced roles, sorted.
    pub fn roles(&self) -> Vec<&str> {
        let mut roles: Vec<&str> = self.colors.iter().map(|entry| entry.role.as_str()).collect();
        roles.sort_unstable();
        roles.dedup();
        roles
    }
}

/// Resolves every `$role` color in `project`, rewriting fields in place.
///
/// Unknown roles, duplicate declarations, palette values that are not concrete
/// colors and scene overrides of locked roles are explicit `invalid_scene`
/// errors. Concrete colors are left untouched and never appear in the report.
pub fn resolve_design(project: &mut Project) -> Result<DesignReport, Error> {
    let base = project.design.as_ref().map(|design| card_map(&design.colors)).transpose()?;
    let base = base.unwrap_or_default();
    let mut report = DesignReport::default();

    for index in 0..project.scenes.len() {
        let scene_map = match project.scenes[index].design.as_ref() {
            Some(design) => Some(card_map(&design.colors)?),
            None => None,
        };
        let layered = layer(&base, scene_map.as_ref(), index)?;
        // Split the borrow so the scene card and the scene body are independent.
        let (background, items) = {
            let scene = &mut project.scenes[index];
            (&mut scene.background, &mut scene.items)
        };
        resolve_background(background, index, &layered, &mut report)?;
        resolve_items(items, Some(index), &layered, &mut report, &format!("scenes[{index}]"))?;
    }

    for (index, component) in project.components.iter_mut().enumerate() {
        resolve_items(&mut component.items, None, &base, &mut report, &format!("components[{}]", index))?;
    }

    Ok(report)
}

#[derive(Clone)]
struct Resolved {
    value: String,
    source: ColorSource,
    locked: bool,
}

fn card_map(colors: &[ColorEntry]) -> Result<HashMap<String, Resolved>, Error> {
    let mut map = HashMap::with_capacity(colors.len());
    for entry in colors {
        if entry.role.trim().is_empty() {
            return Err(Error::invalid("design: color role must not be empty"));
        }
        if entry.role.starts_with(TOKEN_PREFIX) {
            return Err(Error::invalid(format!("design: color role `{}` must not start with `{TOKEN_PREFIX}`", entry.role)));
        }
        if entry.value.starts_with(TOKEN_PREFIX) {
            return Err(Error::invalid(format!("design: color `{}` must be concrete, not another token", entry.role)));
        }
        if !is_concrete_color(&entry.value) {
            return Err(Error::invalid(format!("design: color `{}` value `{}` is not #rgb, #rrggbb or #rrggbbaa", entry.role, entry.value)));
        }
        if map.insert(entry.role.clone(), Resolved { value: entry.value.clone(), source: ColorSource::Project, locked: entry.locked }).is_some()
        {
            return Err(Error::invalid(format!("design: duplicate color role `{}`", entry.role)));
        }
    }
    Ok(map)
}

fn layer(
    base: &HashMap<String, Resolved>,
    scene: Option<&HashMap<String, Resolved>>,
    scene_index: usize,
) -> Result<HashMap<String, Resolved>, Error> {
    let mut merged = (*base).clone();
    let Some(scene) = scene
    else {
        return Ok(merged);
    };
    for (role, entry) in scene {
        if let Some(base_entry) = base.get(role) {
            if base_entry.locked && base_entry.value != entry.value {
                return Err(Error::invalid(format!(
                    "design: scene {scene_index} overrides locked color `{role}` ({base_value} -> {scene_value})",
                    base_value = base_entry.value,
                    scene_value = entry.value
                )));
            }
        }
        merged.insert(role.clone(), Resolved { value: entry.value.clone(), source: ColorSource::Scene, locked: entry.locked });
    }
    Ok(merged)
}

fn is_concrete_color(value: &str) -> bool {
    let Some(hex) = value.strip_prefix('#')
    else {
        return false;
    };
    matches!(hex.len(), 3 | 6 | 8) && hex.chars().all(|letter| letter.is_ascii_hexdigit())
}

#[allow(clippy::too_many_arguments)]
fn resolve_color(
    value: &mut String,
    scene: Option<usize>,
    node: Option<&NodeId>,
    path: &str,
    map: &HashMap<String, Resolved>,
    report: &mut DesignReport,
) -> Result<(), Error> {
    let Some(role) = value.strip_prefix(TOKEN_PREFIX).map(str::to_string)
    else {
        return Ok(());
    };
    if role.is_empty() {
        return Err(Error::invalid(format!("design: empty color token at {path}")));
    }
    let Some(resolved) = map.get(&role)
    else {
        return Err(Error::invalid(format!("design: unknown color `{role}` at {path}")));
    };
    *value = resolved.value.clone();
    report.colors.push(ColorRef {
        role,
        value: resolved.value.clone(),
        source: resolved.source,
        scene,
        node: node.cloned(),
        path: path.to_string(),
    });
    Ok(())
}

fn resolve_optional(
    value: &mut Option<String>,
    scene: Option<usize>,
    node: Option<&NodeId>,
    path: &str,
    map: &HashMap<String, Resolved>,
    report: &mut DesignReport,
) -> Result<(), Error> {
    if let Some(inner) = value {
        resolve_color(inner, scene, node, path, map, report)?;
    }
    Ok(())
}

fn resolve_background(
    background: &mut Background,
    scene: usize,
    map: &HashMap<String, Resolved>,
    report: &mut DesignReport,
) -> Result<(), Error> {
    match background {
        Background::Solid { color } => {
            resolve_color(color, Some(scene), None, &format!("scenes[{scene}].background.color"), map, report)?;
        }
        Background::VGradient { colors } => {
            for (index, color) in colors.iter_mut().enumerate() {
                let path = format!("scenes[{scene}].background.colors[{index}]");
                resolve_color(color, Some(scene), None, &path, map, report)?;
            }
        }
        Background::Radial { center, edge } => {
            resolve_color(center, Some(scene), None, &format!("scenes[{scene}].background.center"), map, report)?;
            resolve_color(edge, Some(scene), None, &format!("scenes[{scene}].background.edge"), map, report)?;
        }
        Background::Starfield { base, tint, .. } => {
            for (index, color) in base.iter_mut().enumerate() {
                let path = format!("scenes[{scene}].background.base[{index}]");
                resolve_color(color, Some(scene), None, &path, map, report)?;
            }
            resolve_optional(tint, Some(scene), None, &format!("scenes[{scene}].background.tint"), map, report)?;
        }
    }
    Ok(())
}

fn resolve_items(
    items: &mut [Node],
    scene: Option<usize>,
    map: &HashMap<String, Resolved>,
    report: &mut DesignReport,
    base_path: &str,
) -> Result<(), Error> {
    for (index, node) in items.iter_mut().enumerate() {
        let mut path = format!("{base_path}.items[{index}]");
        if let Some(id) = &node.id {
            let _ = write!(path, "({id})");
        }
        resolve_item(&mut node.item, scene, node.id.as_ref(), &path, map, report)?;
    }
    Ok(())
}

fn resolve_item(
    item: &mut Item,
    scene: Option<usize>,
    node: Option<&NodeId>,
    base: &str,
    map: &HashMap<String, Resolved>,
    report: &mut DesignReport,
) -> Result<(), Error> {
    match item {
        Item::Text { color, sub_color, glow, .. } => {
            resolve_color(color, scene, node, &format!("{base}.color"), map, report)?;
            resolve_optional(sub_color, scene, node, &format!("{base}.sub_color"), map, report)?;
            resolve_optional(glow, scene, node, &format!("{base}.glow"), map, report)?;
        }
        Item::Code { theme, .. } => {
            resolve_optional(&mut theme.panel, scene, node, &format!("{base}.theme.panel"), map, report)?;
            resolve_color(&mut theme.plain, scene, node, &format!("{base}.theme.plain"), map, report)?;
            resolve_color(&mut theme.keyword, scene, node, &format!("{base}.theme.keyword"), map, report)?;
            resolve_color(&mut theme.string, scene, node, &format!("{base}.theme.string"), map, report)?;
            resolve_color(&mut theme.comment, scene, node, &format!("{base}.theme.comment"), map, report)?;
            resolve_color(&mut theme.number, scene, node, &format!("{base}.theme.number"), map, report)?;
            resolve_color(&mut theme.function, scene, node, &format!("{base}.theme.function"), map, report)?;
        }
        Item::Counter { color, .. } | Item::Shape { color, .. } | Item::Bands { color, .. } => {
            resolve_color(color, scene, node, &format!("{base}.color"), map, report)?;
        }
        Item::Card { accent, panel, .. } => {
            resolve_optional(accent, scene, node, &format!("{base}.accent"), map, report)?;
            resolve_optional(panel, scene, node, &format!("{base}.panel"), map, report)?;
        }
        Item::Path { fill, color, .. } => {
            resolve_optional(fill, scene, node, &format!("{base}.fill"), map, report)?;
            resolve_color(color, scene, node, &format!("{base}.color"), map, report)?;
        }
        Item::Layer { items, .. } => {
            resolve_items(items, scene, map, report, base)?;
        }
        Item::Glow { color, .. } | Item::Footnote { color, .. } => {
            resolve_color(color, scene, node, &format!("{base}.color"), map, report)?;
        }
        Item::Progress { track, fill, .. } => {
            resolve_optional(track, scene, node, &format!("{base}.track"), map, report)?;
            resolve_optional(fill, scene, node, &format!("{base}.fill"), map, report)?;
        }
        Item::Chart { bars, lines, axis, grid, .. } => {
            for (index, series) in bars.iter_mut().enumerate() {
                let path = format!("{base}.bars[{index}].color");
                resolve_color(&mut series.color, scene, node, &path, map, report)?;
            }
            for (index, series) in lines.iter_mut().enumerate() {
                let path = format!("{base}.lines[{index}].color");
                resolve_color(&mut series.color, scene, node, &path, map, report)?;
            }
            resolve_optional(axis, scene, node, &format!("{base}.axis"), map, report)?;
            resolve_optional(grid, scene, node, &format!("{base}.grid"), map, report)?;
        }
        Item::Callout { color, text_color, panel, .. } => {
            resolve_optional(color, scene, node, &format!("{base}.color"), map, report)?;
            resolve_optional(text_color, scene, node, &format!("{base}.text_color"), map, report)?;
            resolve_optional(panel, scene, node, &format!("{base}.panel"), map, report)?;
        }
        Item::Grain { .. } | Item::Instance { .. } | Item::Image { .. } | Item::Video { .. } => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(json: serde_json::Value) -> Project {
        serde_json::from_value(json).expect("project parses")
    }

    fn base_project(scene_design: Option<serde_json::Value>, color: &str) -> serde_json::Value {
        let mut scene = serde_json::json!({
            "start": 0,
            "duration": 30,
            "background": { "kind": "solid", "color": color },
            "items": [{
                "kind": "text", "at": 0, "dur": 30, "content": "MV", "size": 40.0,
                "color": color, "x": 0.5, "y": 0.5
            }]
        });
        if let Some(design) = scene_design {
            scene["design"] = design;
        }
        serde_json::json!({
            "width": 320,
            "height": 180,
            "fps": 30,
            "duration_frames": 30,
            "scenes": [scene],
            "design": {
                "colors": [
                    { "role": "ink", "value": "#e6eefa" },
                    { "role": "accent", "value": "#7aa2ff", "locked": true }
                ]
            }
        })
    }

    #[test]
    fn project_tokens_rewrite_fields_and_report_references() {
        let mut project = project(base_project(None, "$accent"));
        let report = resolve_design(&mut project).expect("design resolves");
        assert_eq!(report.colors.len(), 2);
        assert_eq!(report.roles(), vec!["accent"]);
        assert_eq!(report.affected_scenes("accent"), vec![0]);
        assert!(report.colors.iter().all(|entry| entry.value == "#7aa2ff"));
        match &project.scenes[0].background {
            Background::Solid { color } => assert_eq!(color, "#7aa2ff"),
            other => panic!("unexpected background: {other:?}"),
        }
        match &project.scenes[0].items[0].item {
            Item::Text { color, .. } => assert_eq!(color, "#7aa2ff"),
            other => panic!("unexpected item: {other:?}"),
        }
    }

    #[test]
    fn scene_card_overrides_unlocked_roles_only() {
        let design = serde_json::json!({ "colors": [{ "role": "ink", "value": "#ffffff" }] });
        let mut project = project(base_project(Some(design), "$ink"));
        let report = resolve_design(&mut project).expect("scene override resolves");
        assert!(report.colors.iter().all(|entry| entry.source == ColorSource::Scene));
        assert!(report.colors.iter().all(|entry| entry.value == "#ffffff"));
    }

    #[test]
    fn locked_role_rejects_scene_override() {
        let design = serde_json::json!({ "colors": [{ "role": "accent", "value": "#ff0000" }] });
        let mut project = project(base_project(Some(design), "$accent"));
        let error = resolve_design(&mut project).expect_err("locked role must not be overridden");
        assert_eq!(error.code(), "invalid_scene");
        assert!(error.message.contains("locked"), "{error}");
        assert!(error.message.contains("accent"), "{error}");
        assert!(error.message.contains("scene 0"), "{error}");
    }

    #[test]
    fn unknown_tokens_are_explicit_errors_with_location() {
        let mut project = project(base_project(None, "$missing"));
        let error = resolve_design(&mut project).expect_err("unknown role");
        assert_eq!(error.code(), "invalid_scene");
        assert!(error.message.contains("unknown color `missing`"), "{error}");
        assert!(error.message.contains("scenes[0].background.color"), "{error}");
    }

    #[test]
    fn duplicate_roles_and_non_color_values_are_rejected() {
        let mut duplicate = project(serde_json::json!({
            "width": 320, "height": 180, "fps": 30, "duration_frames": 30, "scenes": [],
            "design": { "colors": [
                { "role": "ink", "value": "#fff" },
                { "role": "ink", "value": "#000" }
            ] }
        }));
        let error = resolve_design(&mut duplicate).expect_err("duplicate role");
        assert!(error.message.contains("duplicate"), "{error}");

        let mut bad_value = project(serde_json::json!({
            "width": 320, "height": 180, "fps": 30, "duration_frames": 30, "scenes": [],
            "design": { "colors": [{ "role": "ink", "value": "not-a-color" }] }
        }));
        let error = resolve_design(&mut bad_value).expect_err("invalid value");
        assert!(error.message.contains("not #rgb"), "{error}");
    }

    #[test]
    fn nested_layers_and_components_report_their_own_paths() {
        let json = serde_json::json!({
            "width": 320, "height": 180, "fps": 30, "duration_frames": 30,
            "scenes": [{
                "start": 0, "duration": 30,
                "items": [{
                    "kind": "layer", "at": 0, "dur": 30, "zoom_from": 1.0, "zoom_to": 1.0,
                    "items": [{ "kind": "glow", "at": 0, "dur": 30, "x": 0.5, "y": 0.5,
                                "radius": 0.3, "color": "$accent", "id": "halo" }]
                }]
            }],
            "components": [{
                "id": "badge",
                "items": [{ "kind": "shape", "at": 0, "dur": 10, "shape": "rect",
                            "x": 0.1, "y": 0.1, "w": 0.2, "h": 0.2, "color": "$accent" }]
            }],
            "design": { "colors": [{ "role": "accent", "value": "#7aa2ff" }] }
        });
        let mut project = project(json);
        let report = resolve_design(&mut project).expect("nested design resolves");
        let scene_ref = report.colors.iter().find(|entry| entry.scene == Some(0)).expect("scene ref");
        assert!(scene_ref.path.contains("items[0].items[0](halo)"), "{}", scene_ref.path);
        assert!(scene_ref.path.ends_with(".color"), "{}", scene_ref.path);
        assert_eq!(scene_ref.node.as_ref().map(NodeId::as_str), Some("halo"));
        let component_ref = report.colors.iter().find(|entry| entry.scene.is_none()).expect("component ref");
        assert!(component_ref.path.starts_with("components[0]"), "{}", component_ref.path);
    }

    #[test]
    fn concrete_colors_are_untouched_and_unreported() {
        let mut project = project(base_project(None, "#123456"));
        let report = resolve_design(&mut project).expect("no tokens is fine");
        assert!(report.colors.is_empty());
        assert!(report.roles().is_empty());
        match &project.scenes[0].background {
            Background::Solid { color } => assert_eq!(color, "#123456"),
            other => panic!("unexpected background: {other:?}"),
        }
    }
}
