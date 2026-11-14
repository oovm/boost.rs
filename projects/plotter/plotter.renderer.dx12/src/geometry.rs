//! Scene IR to triangle list.
//!
//! Scene coordinates are pixels with the origin at the bottom-left corner while
//! [`hardware_backend_dx12::Vertex`] expects the origin at the top-left corner,
//! so every point is flipped on the way out. Emitted triangles keep document
//! order paint order, and each 3D node sorts its own triangles far to near.

use hardware_backend_dx12::Vertex;
use plotter_types::plot::{NodeId, Rgba, Vec3};
use plotter_view::{
    AxisNode, Mesh3Node, MeshNode, Point2, Points3Node, PointsNode, PolylineNode, Scene, SceneNodeKind, ScreenPoint, try_project_to_screen,
};

use crate::error::RenderError;

/// Smallest stroke width the raster path honours, in pixels.
const MIN_STROKE_WIDTH: f32 = 1.0;

/// Axis tick length in pixels.
const TICK_LENGTH: f64 = 6.0;

/// Convert every drawable node reachable from `scene.root` into a triangle list.
pub(crate) fn collect_triangles(scene: &Scene, height: u32) -> Result<Vec<Vertex>, RenderError> {
    let mut builder = Builder { height: height as f32, vertices: Vec::new() };
    builder.visit(scene, scene.root)?;
    Ok(builder.vertices)
}

/// Accumulates triangle vertices in paint order.
struct Builder {
    height: f32,
    vertices: Vec<Vertex>,
}

impl Builder {
    fn visit(&mut self, scene: &Scene, id: NodeId) -> Result<(), RenderError> {
        let node = scene.nodes.get(id).ok_or(RenderError::MissingNode { node: id.0 })?;
        match &node.kind {
            SceneNodeKind::Group { children } => {
                for child in children {
                    self.visit(scene, *child)?;
                }
            }
            SceneNodeKind::Polyline(polyline) => self.push_polyline(polyline),
            SceneNodeKind::Points(points) => self.push_points(points),
            SceneNodeKind::Mesh(mesh) => self.push_mesh(mesh, node.id)?,
            SceneNodeKind::Mesh3(mesh) => self.push_mesh3(scene, mesh, node.id)?,
            SceneNodeKind::Points3(points) => self.push_points3(scene, points),
            SceneNodeKind::Axis(axis) => self.push_axis(axis),
            SceneNodeKind::Text(_) => return Err(RenderError::unsupported("text", node.id.0)),
        }
        Ok(())
    }

    fn pixel(&self, point: Point2) -> [f32; 2] {
        [point.x as f32, self.height - point.y as f32]
    }

    fn screen_pixel(&self, point: ScreenPoint) -> [f32; 2] {
        [point.x as f32, self.height - point.y as f32]
    }

    fn push_triangle(&mut self, corner: [[f32; 2]; 3], color: Rgba) {
        let color = [color.r, color.g, color.b, color.a];
        for position in corner {
            self.vertices.push(Vertex { position, color });
        }
    }

    /// Emit a quad of `linewidth` pixels around the segment `a`-`b`.
    fn push_segment(&mut self, a: [f32; 2], b: [f32; 2], color: Rgba, linewidth: f32) {
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let length = (dx * dx + dy * dy).sqrt();
        let half = linewidth.max(MIN_STROKE_WIDTH) * 0.5;
        let (nx, ny) = if length > f32::EPSILON { (-dy / length * half, dx / length * half) } else { (0.0, half) };
        let corners = [[a[0] + nx, a[1] + ny], [b[0] + nx, b[1] + ny], [b[0] - nx, b[1] - ny], [a[0] - nx, a[1] - ny]];
        self.push_triangle([corners[0], corners[1], corners[2]], color);
        self.push_triangle([corners[0], corners[2], corners[3]], color);
    }

    /// Emit a plus-sign marker centred on `center` with arm length `radius`.
    fn push_cross(&mut self, center: [f32; 2], radius: f32, color: Rgba) {
        let radius = radius.max(0.5);
        self.push_segment([center[0] - radius, center[1]], [center[0] + radius, center[1]], color, MIN_STROKE_WIDTH);
        self.push_segment([center[0], center[1] - radius], [center[0], center[1] + radius], color, MIN_STROKE_WIDTH);
    }

    fn push_polyline(&mut self, polyline: &PolylineNode) {
        for window in polyline.points.windows(2) {
            let a = self.pixel(window[0]);
            let b = self.pixel(window[1]);
            self.push_segment(a, b, polyline.stroke, polyline.linewidth);
        }
    }

    fn push_points(&mut self, points: &PointsNode) {
        let radius = points.size;
        for position in &points.positions {
            let center = self.pixel(*position);
            self.push_cross(center, radius, points.fill);
        }
    }

    fn push_mesh(&mut self, mesh: &MeshNode, id: NodeId) -> Result<(), RenderError> {
        for triangle in mesh.indices.as_chunks::<3>().0 {
            let corners = [
                self.pixel(mesh_position(&mesh.positions, triangle[0], id)?),
                self.pixel(mesh_position(&mesh.positions, triangle[1], id)?),
                self.pixel(mesh_position(&mesh.positions, triangle[2], id)?),
            ];
            self.push_triangle(corners, mesh.fill);
        }
        Ok(())
    }

    fn push_mesh3(&mut self, scene: &Scene, mesh: &Mesh3Node, id: NodeId) -> Result<(), RenderError> {
        let mut projected = Vec::new();
        for triangle in mesh.indices.as_chunks::<3>().0 {
            let a = mesh3_position(&mesh.positions, triangle[0], id)?;
            let b = mesh3_position(&mesh.positions, triangle[1], id)?;
            let c = mesh3_position(&mesh.positions, triangle[2], id)?;
            let (Some(a), Some(b), Some(c)) = (
                try_project_to_screen(&scene.camera, scene.viewport, *a),
                try_project_to_screen(&scene.camera, scene.viewport, *b),
                try_project_to_screen(&scene.camera, scene.viewport, *c),
            )
            else {
                continue;
            };
            let depth = (a.depth + b.depth + c.depth) / 3.0;
            projected.push((depth, [self.screen_pixel(a), self.screen_pixel(b), self.screen_pixel(c)]));
        }
        projected.sort_by(|left, right| right.0.total_cmp(&left.0));
        for (_, corners) in projected {
            self.push_triangle(corners, mesh.fill);
        }
        Ok(())
    }

    fn push_points3(&mut self, scene: &Scene, points: &Points3Node) {
        for position in &points.positions {
            let Some(projected) = try_project_to_screen(&scene.camera, scene.viewport, *position)
            else {
                continue;
            };
            let center = self.screen_pixel(projected);
            self.push_cross(center, points.size, points.fill);
        }
    }

    fn push_axis(&mut self, axis: &AxisNode) {
        let origin = axis.origin;
        let end = if axis.horizontal { Point2::new(origin.x + axis.length, origin.y) } else { Point2::new(origin.x, origin.y + axis.length) };
        self.push_segment(self.pixel(origin), self.pixel(end), axis.stroke, MIN_STROKE_WIDTH);

        let ticks = axis.tick_count.max(2);
        for i in 0..ticks {
            let t = f64::from(i) / f64::from(ticks - 1);
            let (start, tip) = if axis.horizontal {
                let x = origin.x + axis.length * t;
                (Point2::new(x, origin.y), Point2::new(x, origin.y - TICK_LENGTH))
            }
            else {
                let y = origin.y + axis.length * t;
                (Point2::new(origin.x, y), Point2::new(origin.x - TICK_LENGTH, y))
            };
            self.push_segment(self.pixel(start), self.pixel(tip), axis.stroke, MIN_STROKE_WIDTH);
        }
    }
}

fn mesh_position(positions: &[Point2], index: u32, id: NodeId) -> Result<Point2, RenderError> {
    positions.get(index as usize).copied().ok_or(RenderError::InvalidIndex { node: id.0, index })
}

fn mesh3_position(positions: &[Vec3], index: u32, id: NodeId) -> Result<&Vec3, RenderError> {
    positions.get(index as usize).ok_or(RenderError::InvalidIndex { node: id.0, index })
}
