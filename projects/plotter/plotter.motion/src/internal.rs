//! Traversal and value helpers shared by planning and sampling.

use std::collections::{HashMap, HashSet};

use plotter_scene::{DisplayScene, NodeKind, PathCommand, PathData, Point, Rect, SceneNode, Transform};

/// Linear interpolation between two scalars with exact endpoints.
///
/// `t <= 0` returns `from` and `t >= 1` returns `to` without arithmetic, so
/// endpoints are bit-exact even when the arithmetic would round differently.
pub(crate) fn lerp(from: f64, to: f64, t: f64) -> f64 {
    if t <= 0.0 {
        from
    }
    else if t >= 1.0 {
        to
    }
    else {
        from + (to - from) * t
    }
}

/// Component-wise linear interpolation of two affine matrices.
///
/// `plotter_types::Transform` intentionally has no interpolation operator, so
/// motion owns this helper. Rotation and scale are interpolated as matrix
/// components rather than decomposed, which keeps the result deterministic and
/// makes the interpolation contract explicit.
pub(crate) fn lerp_transform(from: Transform, to: Transform, t: f64) -> Transform {
    Transform::new(
        lerp(from.a(), to.a(), t),
        lerp(from.b(), to.b(), t),
        lerp(from.c(), to.c(), t),
        lerp(from.d(), to.d(), t),
        lerp(from.e(), to.e(), t),
        lerp(from.f(), to.f(), t),
    )
}

/// The node's own opacity, independent of its ancestors.
///
/// The scene model stores opacity on the variant that owns it: groups and
/// images carry an `opacity` field, while shapes and text store it in their
/// style. Motion treats these as one animatable opacity channel.
pub(crate) fn node_opacity(node: &SceneNode) -> f64 {
    match &node.kind {
        NodeKind::Group { opacity, .. } | NodeKind::Image { opacity, .. } => *opacity,
        NodeKind::Path { style, .. }
        | NodeKind::Line { style, .. }
        | NodeKind::Rect { style, .. }
        | NodeKind::RoundedRect { style, .. }
        | NodeKind::Ellipse { style, .. }
        | NodeKind::Marker { style, .. } => style.opacity,
        NodeKind::Text { style, .. } => style.opacity,
    }
}

/// Set the node's own opacity, clamped to `0.0..=1.0`.
pub(crate) fn set_node_opacity(node: &mut SceneNode, value: f64) {
    let value = value.clamp(0.0, 1.0);
    match &mut node.kind {
        NodeKind::Group { opacity, .. } | NodeKind::Image { opacity, .. } => *opacity = value,
        NodeKind::Path { style, .. }
        | NodeKind::Line { style, .. }
        | NodeKind::Rect { style, .. }
        | NodeKind::RoundedRect { style, .. }
        | NodeKind::Ellipse { style, .. }
        | NodeKind::Marker { style, .. } => style.opacity = value,
        NodeKind::Text { style, .. } => style.opacity = value,
    }
}

/// Scale the node's own opacity by `factor`, clamped to `0.0..=1.0`.
pub(crate) fn multiply_node_opacity(node: &mut SceneNode, factor: f64) {
    set_node_opacity(node, node_opacity(node) * factor);
}

/// The rectangle carried by a shape, if the node kind has one.
///
/// Only `Rect` and `RoundedRect` own a rectangle. Every other kind reports
/// `None`, so they never receive an extent track and are never silently
/// converted into one.
pub(crate) fn node_extent(node: &SceneNode) -> Option<Rect> {
    match &node.kind {
        NodeKind::Rect { rect, .. } | NodeKind::RoundedRect { rect, .. } => Some(*rect),
        _ => None,
    }
}

/// Replace the rectangle of a shape, leaving other kinds untouched.
///
/// Takes the kind rather than the whole node so sampling can mutate geometry
/// while it still borrows the node's id.
pub(crate) fn set_node_extent(kind: &mut NodeKind, value: Rect) {
    match kind {
        NodeKind::Rect { rect, .. } | NodeKind::RoundedRect { rect, .. } => *rect = value,
        _ => {}
    }
}

/// Trim a path to the first `fraction` of its total length.
///
/// `fraction >= 1` returns the path unchanged, `fraction <= 0` returns an empty
/// path. Curves are flattened with a fixed subdivision before measuring, so the
/// result is deterministic and always a polyline; a partially trimmed subpath
/// drops its `Close`, because closing it would draw a chord that was never part
/// of the visible prefix.
pub(crate) fn trim_path(path: &PathData, fraction: f64) -> PathData {
    if fraction >= 1.0 || path.commands.is_empty() {
        return path.clone();
    }
    if fraction <= 0.0 {
        return PathData::new();
    }
    let subpaths = flatten(path);
    let total: f64 = subpaths.iter().map(|(points, _)| length(points)).sum();
    if !total.is_finite() || total <= 0.0 {
        return path.clone();
    }

    let mut budget = total * fraction;
    let mut out = PathData::new();
    for (points, closed) in &subpaths {
        let Some(first) = points.first().copied() else {
            continue;
        };
        out.commands.push(PathCommand::MoveTo(first));
        for pair in points.windows(2) {
            if budget <= 0.0 {
                return out;
            }
            let (from, to) = (pair[0], pair[1]);
            let segment = distance(from, to);
            if budget >= segment {
                out.commands.push(PathCommand::LineTo(to));
                budget -= segment;
            }
            else {
                let t = if segment > 0.0 { budget / segment } else { 0.0 };
                out.commands.push(PathCommand::LineTo(lerp_point(from, to, t)));
                return out;
            }
        }
        if *closed && budget > 0.0 {
            let last = *points.last().expect("a subpath always has a point");
            let closing = distance(last, first);
            if budget < closing {
                out.commands.push(PathCommand::LineTo(lerp_point(last, first, budget / closing)));
                return out;
            }
            out.commands.push(PathCommand::Close);
            budget -= closing;
        }
    }
    out
}

/// Trim every `Path` node in a subtree, leaving other kinds untouched.
pub(crate) fn trim_subtree(node: &mut SceneNode, fraction: f64) {
    match &mut node.kind {
        NodeKind::Path { path, .. } => *path = trim_path(path, fraction),
        NodeKind::Group { children, .. } => {
            for child in children {
                trim_subtree(child, fraction);
            }
        }
        _ => {}
    }
}

/// Flatten a path into polylines plus a closed flag per subpath.
///
/// Quadratic and cubic segments use a fixed 12-step subdivision, matching the
/// documented "flattened, deterministic" contract of [`trim_path`].
fn flatten(path: &PathData) -> Vec<(Vec<Point>, bool)> {
    const STEPS: usize = 12;
    let mut subpaths = Vec::new();
    let mut current: Vec<Point> = Vec::new();
    let mut closed = false;
    let mut last = Point::new(0.0, 0.0);

    for command in &path.commands {
        match *command {
            PathCommand::MoveTo(point) => {
                if !current.is_empty() {
                    subpaths.push((std::mem::take(&mut current), closed));
                }
                closed = false;
                current.push(point);
                last = point;
            }
            PathCommand::LineTo(point) => {
                current.push(point);
                last = point;
            }
            PathCommand::QuadraticTo { control, to } => {
                let from = last;
                for step in 1..=STEPS {
                    let t = step as f64 / STEPS as f64;
                    current.push(quadratic(from, control, to, t));
                }
                last = to;
            }
            PathCommand::CubicTo { control1, control2, to } => {
                let from = last;
                for step in 1..=STEPS {
                    let t = step as f64 / STEPS as f64;
                    current.push(cubic(from, control1, control2, to, t));
                }
                last = to;
            }
            PathCommand::Close => closed = true,
        }
    }
    if !current.is_empty() {
        subpaths.push((current, closed));
    }
    subpaths
}

fn distance(from: Point, to: Point) -> f64 {
    (to.x - from.x).hypot(to.y - from.y)
}

fn length(points: &[Point]) -> f64 {
    points.windows(2).map(|pair| distance(pair[0], pair[1])).sum()
}

fn lerp_point(from: Point, to: Point, t: f64) -> Point {
    Point::new(lerp(from.x, to.x, t), lerp(from.y, to.y, t))
}

fn quadratic(from: Point, control: Point, to: Point, t: f64) -> Point {
    let u = 1.0 - t;
    Point::new(
        u * u * from.x + 2.0 * u * t * control.x + t * t * to.x,
        u * u * from.y + 2.0 * u * t * control.y + t * t * to.y,
    )
}

fn cubic(from: Point, control1: Point, control2: Point, to: Point, t: f64) -> Point {
    let u = 1.0 - t;
    Point::new(
        u * u * u * from.x + 3.0 * u * u * t * control1.x + 3.0 * u * t * t * control2.x + t * t * t * to.x,
        u * u * u * from.y + 3.0 * u * u * t * control1.y + 3.0 * u * t * t * control2.y + t * t * t * to.y,
    )
}

/// First node for every id in pre-order document order.
///
/// Duplicate ids keep the first occurrence, which is the contract every other
/// operation in this crate uses.
pub(crate) fn first_nodes(scene: &DisplayScene) -> HashMap<&str, &SceneNode> {
    let mut nodes = HashMap::new();
    for node in &scene.nodes {
        collect_first_nodes(node, &mut nodes);
    }
    nodes
}

fn collect_first_nodes<'a>(node: &'a SceneNode, nodes: &mut HashMap<&'a str, &'a SceneNode>) {
    if let Some(id) = node.id.as_deref() {
        nodes.entry(id).or_insert(node);
    }
    if let NodeKind::Group { children, .. } = &node.kind {
        for child in children {
            collect_first_nodes(child, nodes);
        }
    }
}

/// Index path of the first occurrence of every id, where a root node is a
/// one-element path and each further element is a child index inside a group.
pub(crate) fn first_paths(scene: &DisplayScene) -> HashMap<&str, Vec<usize>> {
    let mut paths = HashMap::new();
    for (index, node) in scene.nodes.iter().enumerate() {
        let mut path = vec![index];
        collect_first_paths(node, &mut path, &mut paths);
    }
    paths
}

fn collect_first_paths<'a>(node: &'a SceneNode, path: &mut Vec<usize>, paths: &mut HashMap<&'a str, Vec<usize>>) {
    if let Some(id) = node.id.as_deref() {
        paths.entry(id).or_insert_with(|| path.clone());
    }
    if let NodeKind::Group { children, .. } = &node.kind {
        for (index, child) in children.iter().enumerate() {
            path.push(index);
            collect_first_paths(child, path, paths);
            path.pop();
        }
    }
}

/// Ids in pre-order document order, keeping only the first occurrence.
pub(crate) fn ordered_ids(scene: &DisplayScene) -> Vec<&str> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for node in &scene.nodes {
        collect_ordered_ids(node, &mut seen, &mut ids);
    }
    ids
}

fn collect_ordered_ids<'a>(node: &'a SceneNode, seen: &mut HashSet<&'a str>, ids: &mut Vec<&'a str>) {
    if let Some(id) = node.id.as_deref() {
        if seen.insert(id) {
            ids.push(id);
        }
    }
    if let NodeKind::Group { children, .. } = &node.kind {
        for child in children {
            collect_ordered_ids(child, seen, ids);
        }
    }
}
