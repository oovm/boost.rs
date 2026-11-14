//! Sampling a transition plan at a point in time.

use std::collections::HashMap;

use plotter_scene::{DisplayScene, NodeKind, Rect, SceneNode, Transform};

use crate::{
    internal::{
        first_paths, lerp, lerp_transform, multiply_node_opacity, set_node_extent, set_node_opacity, trim_path,
        trim_subtree,
    },
    plan::{Channel, ChannelValue, CorrespondenceKind, EnterPolicy, ExitPolicy, Track, TransitionPlan},
};

/// Sample a plan at `time` and return a drawable scene.
///
/// `time` is clamped to `0.0..=duration`. When `duration <= 0.0` the next scene
/// is returned unchanged, so "no animation" is exactly the final state.
///
/// The output tree is the next scene's tree. `Update` nodes interpolate their
/// tracked `Transform` and `Opacity` values with `intent.ease`; `Enter` nodes
/// either appear immediately (`Snap`) or fade their own opacity from `0.0`
/// (`Fade`). Nodes without tracked channels keep the next scene's value. When a
/// plan repeats an id, the first correspondence and the first track for a
/// channel win, mirroring the planning contract.
///
/// Exiting nodes are cloned from the previous scene and appended after the
/// existing content, so they paint above it. When an exiting node has a matched
/// ancestor that survives as an `Update`, it is appended to that ancestor's
/// children instead, preserving the ancestor's transform composition. An
/// exiting subtree is moved as a whole; its descendants are not visited
/// separately. `Snap` never appends it, `Fade` appends it with its opacity
/// scaled by `1 - progress` while time is before the duration, and `Keep`
/// appends it unchanged at every sampled time.
///
/// # Known boundaries
///
/// - `Transform`, `Extent` and `Opacity` differ between frames. Path commands,
///   text content, clips, and colors are snapped to the next scene, or copied
///   verbatim for exiting nodes. Shape morphing and color interpolation are not
///   implemented; an extent track only exists for `Rect` and `RoundedRect`
///   nodes that also exist in both scenes.
/// - Transform interpolation is component-wise; rotations and non-uniform
///   scales therefore do not decompose.
/// - Entering and exiting nodes animate opacity or path length only.
///   `intent.channels` applies to `Update` nodes, while `intent.enter` and
///   `intent.exit` own their own opacity and trim behavior. `Trim` and `Retract`
///   only touch `Path` nodes; other kinds snap.
/// - Nodes without ids are never corresponded; id-less nodes that only exist in
///   the previous scene are not drawn.
pub fn sample(plan: &TransitionPlan, previous: &DisplayScene, next: &DisplayScene, time: f64) -> DisplayScene {
    let duration = plan.intent.duration;
    if duration <= 0.0 {
        return next.clone();
    }

    let progress = plan.intent.ease.apply(time.clamp(0.0, duration) / duration);
    let index = SampleIndex::new(plan, next);
    let mut scene = next.clone();
    for node in &mut scene.nodes {
        apply_next(node, &index, plan.intent.enter, progress, None, None);
    }

    let leftover = 1.0 - progress;
    let mut exits = Vec::new();
    let mut ancestors = Vec::new();
    for node in &previous.nodes {
        collect_exits(node, &index, &mut ancestors, &mut exits);
    }
    for (host, node) in exits {
        let visibility = match plan.intent.exit {
            ExitPolicy::Snap => continue,
            ExitPolicy::Fade if leftover <= 0.0 => continue,
            ExitPolicy::Fade => leftover,
            ExitPolicy::Retract if leftover <= 0.0 => continue,
            ExitPolicy::Retract => leftover,
            ExitPolicy::Keep => 1.0,
        };
        let mut node = node.clone();
        if plan.intent.exit == ExitPolicy::Retract {
            trim_subtree(&mut node, leftover);
        }
        else if visibility != 1.0 {
            multiply_node_opacity(&mut node, visibility);
        }
        append_node(&mut scene, host.as_deref(), node);
    }

    scene
}

/// Lookup tables built once per sample call. Hash maps are used for membership
/// and value lookup only; every emitted order comes from document-order walks.
struct SampleIndex<'a> {
    kinds: HashMap<&'a str, CorrespondenceKind>,
    tracks: HashMap<(&'a str, Channel), &'a Track>,
    next_paths: HashMap<&'a str, Vec<usize>>,
}

impl<'a> SampleIndex<'a> {
    fn new(plan: &'a TransitionPlan, next: &'a DisplayScene) -> Self {
        let mut kinds = HashMap::new();
        for correspondence in &plan.correspondences {
            kinds.entry(correspondence.id.as_str()).or_insert(correspondence.kind);
        }
        let mut tracks = HashMap::new();
        for track in &plan.tracks {
            tracks.entry((track.id.as_str(), track.channel)).or_insert(track);
        }
        Self { kinds, tracks, next_paths: first_paths(next) }
    }
}

fn apply_next(
    node: &mut SceneNode,
    index: &SampleIndex<'_>,
    enter: EnterPolicy,
    progress: f64,
    inherited_fade: Option<f64>,
    inherited_trim: Option<f64>,
) {
    let mut child_fade = inherited_fade;
    let mut child_trim = inherited_trim;
    if let Some(id) = node.id.as_deref() {
        match index.kinds.get(id).copied() {
            Some(CorrespondenceKind::Enter) => {
                let local = inherited_fade.is_none() && inherited_trim.is_none();
                match enter {
                    EnterPolicy::Fade if local => {
                        multiply_node_opacity(node, progress);
                        child_fade = Some(progress);
                    }
                    EnterPolicy::Trim if local => child_trim = Some(progress),
                    _ => {}
                }
            }
            Some(CorrespondenceKind::Update) => {
                if let Some(track) = index.tracks.get(&(id, Channel::Transform)) {
                    if let Some(transform) = interpolated_transform(track, progress) {
                        node.transform = transform;
                    }
                }
                if let Some(track) = index.tracks.get(&(id, Channel::Extent)) {
                    if let Some(rect) = interpolated_extent(track, progress) {
                        set_node_extent(&mut node.kind, rect);
                    }
                }
                if let Some(track) = index.tracks.get(&(id, Channel::Opacity)) {
                    if let Some(value) = interpolated_opacity(track, progress) {
                        set_node_opacity(node, value);
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(fraction) = child_trim {
        if let NodeKind::Path { path, .. } = &mut node.kind {
            *path = trim_path(path, fraction);
        }
    }

    if let NodeKind::Group { children, .. } = &mut node.kind {
        for child in children {
            apply_next(child, index, enter, progress, child_fade, child_trim);
        }
    }
}

fn interpolated_transform(track: &Track, progress: f64) -> Option<Transform> {
    match (track.from, track.to) {
        (ChannelValue::Transform(from), ChannelValue::Transform(to)) => Some(lerp_transform(from, to, progress)),
        _ => None,
    }
}

fn interpolated_extent(track: &Track, progress: f64) -> Option<Rect> {
    match (track.from, track.to) {
        (ChannelValue::Extent(from), ChannelValue::Extent(to)) => Some(Rect {
            x: lerp(from.x, to.x, progress),
            y: lerp(from.y, to.y, progress),
            width: lerp(from.width, to.width, progress),
            height: lerp(from.height, to.height, progress),
        }),
        _ => None,
    }
}

fn interpolated_opacity(track: &Track, progress: f64) -> Option<f64> {
    match (track.from, track.to) {
        (ChannelValue::Opacity(from), ChannelValue::Opacity(to)) => Some(lerp(from, to, progress).clamp(0.0, 1.0)),
        _ => None,
    }
}

/// Collect previous-scene subtrees whose id is `Exit`.
///
/// Recursion stops at an exiting node because its whole subtree moves with it.
fn collect_exits<'a>(
    node: &'a SceneNode,
    index: &SampleIndex<'_>,
    ancestors: &mut Vec<&'a str>,
    exits: &mut Vec<(Option<Vec<usize>>, &'a SceneNode)>,
) {
    let id = node.id.as_deref();
    if let Some(id) = id {
        if index.kinds.get(id) == Some(&CorrespondenceKind::Exit) {
            exits.push((exit_host(ancestors, index), node));
            return;
        }
    }

    if let NodeKind::Group { children, .. } = &node.kind {
        if let Some(id) = id {
            ancestors.push(id);
        }
        for child in children {
            collect_exits(child, index, ancestors, exits);
        }
        if id.is_some() {
            ancestors.pop();
        }
    }
}

/// Path of the nearest surviving ancestor, used as the insertion point for an
/// exiting subtree.
fn exit_host(ancestors: &[&str], index: &SampleIndex<'_>) -> Option<Vec<usize>> {
    ancestors.iter().rev().find_map(|id| {
        if index.kinds.get(*id) == Some(&CorrespondenceKind::Update) { index.next_paths.get(*id).cloned() } else { None }
    })
}

/// Append an exiting node at its host path, falling back to the scene roots.
fn append_node(scene: &mut DisplayScene, host: Option<&[usize]>, node: SceneNode) {
    if let Some(host) = host {
        if let Some(parent) = node_at_mut(scene, host) {
            if let NodeKind::Group { children, .. } = &mut parent.kind {
                children.push(node);
                return;
            }
        }
    }
    scene.nodes.push(node);
}

fn node_at_mut<'a>(scene: &'a mut DisplayScene, path: &[usize]) -> Option<&'a mut SceneNode> {
    let (first, rest) = path.split_first()?;
    let mut node = scene.nodes.get_mut(*first)?;
    for index in rest {
        let NodeKind::Group { children, .. } = &mut node.kind
        else {
            return None;
        };
        node = children.get_mut(*index)?;
    }
    Some(node)
}
