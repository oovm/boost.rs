//! Behaviour of the in-house Direct3D 12 scene rasteriser.

use plotter_renderer::RgbaImage;
use plotter_renderer_dx12::{RenderError, is_available, render_rgba8};
use plotter_types::plot::Rgba;
use plotter_view::{MeshNode, Point2, PointsNode, PolylineNode, Rect2, Scene, SceneArena, SceneNodeKind, TextNode, Viewport};

const RED: Rgba = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
const GREEN: Rgba = Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 };

fn scene_with_line_and_mesh() -> Scene {
    let mut arena = SceneArena::new();
    let line = arena.insert(SceneNodeKind::Polyline(PolylineNode {
        points: vec![Point2::new(8.0, 28.0), Point2::new(56.0, 60.0)],
        stroke: RED,
        linewidth: 3.0,
    }));
    let mesh =
        arena.insert(SceneNodeKind::Mesh(MeshNode::from_rect(Rect2 { min: Point2::new(4.0, 4.0), max: Point2::new(60.0, 20.0) }, GREEN)));
    let root = arena.insert(SceneNodeKind::Group { children: vec![mesh, line] });
    Scene::from_arena(root, arena, Viewport::new(64.0, 64.0))
}

fn opaque_pixels_with(image: &RgbaImage, expected: [u8; 4]) -> usize {
    image.pixels.as_chunks::<4>().0.iter().filter(|pixel| **pixel == expected).count()
}

#[test]
fn renders_polyline_and_mesh_through_d3d12() {
    if !is_available() {
        eprintln!("skip plotter d3d12 scene render: no usable Direct3D 12 device on this host");
        return;
    }

    let image = render_rgba8(&scene_with_line_and_mesh(), 64, 64).expect("scene should rasterise");
    assert_eq!((image.width, image.height), (64, 64));
    assert_eq!(image.pixels.len(), 64 * 64 * 4);
    assert!(opaque_pixels_with(&image, [255, 0, 0, 255]) > 0, "the red polyline must reach the target");
    assert!(opaque_pixels_with(&image, [0, 255, 0, 255]) > 0, "the green mesh must reach the target");
    assert!(opaque_pixels_with(&image, [0, 0, 0, 0]) > 0, "untouched pixels stay transparent");
}

#[test]
fn empty_scene_renders_fully_transparent() {
    let mut arena = SceneArena::new();
    let root = arena.insert(SceneNodeKind::Group { children: Vec::new() });
    let scene = Scene::from_arena(root, arena, Viewport::new(64.0, 64.0));

    let image = render_rgba8(&scene, 64, 64).expect("an empty scene is not an error");
    assert_eq!(image.pixels.len(), 64 * 64 * 4);
    assert!(image.pixels.iter().all(|byte| *byte == 0), "an empty scene stays transparent black");
}

#[test]
fn points_render_as_crosses() {
    let mut arena = SceneArena::new();
    let points = arena.insert(SceneNodeKind::Points(PointsNode { positions: vec![Point2::new(32.0, 32.0)], size: 4.0, fill: GREEN }));
    let root = arena.insert(SceneNodeKind::Group { children: vec![points] });
    let scene = Scene::from_arena(root, arena, Viewport::new(64.0, 64.0));

    if !is_available() {
        eprintln!("skip plotter d3d12 point render: no usable Direct3D 12 device on this host");
        return;
    }
    let image = render_rgba8(&scene, 64, 64).expect("points should rasterise");
    // Multisampling leaves thin strokes partially covered, so compare hue and
    // coverage rather than demanding one fully opaque pixel.
    assert!(
        image.pixels.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0 && pixel[1] > pixel[0] && pixel[1] > pixel[2]),
        "the point cross must reach the target"
    );
}

#[test]
fn text_nodes_rasterise_through_system_fonts() {
    let mut arena = SceneArena::new();
    let text = arena.insert(SceneNodeKind::Text(TextNode {
        position: Point2::new(4.0, 44.0),
        content: "Hi".to_string(),
        size: 32.0,
        color: Rgba::WHITE,
    }));
    let root = arena.insert(SceneNodeKind::Group { children: vec![text] });
    let scene = Scene::from_arena(root, arena, Viewport::new(128.0, 64.0));

    let image = match render_rgba8(&scene, 128, 64) {
        Ok(image) => image,
        Err(RenderError::FontUnavailable { .. }) => {
            eprintln!("skip plotter d3d12 text render: no usable system font on this host");
            return;
        }
        Err(error) => panic!("text should rasterise, got {error}"),
    };
    let painted = image.pixels.as_chunks::<4>().0.iter().filter(|pixel| pixel[3] > 0).count();
    assert!(painted > 20, "glyph coverage must reach the target, painted {painted} pixels");
    assert!(
        image.pixels.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 200 && pixel[0] > 200 && pixel[1] > 200),
        "opaque white ink must appear where the glyphs are"
    );
    assert!(opaque_pixels_with(&image, [0, 0, 0, 0]) > 0, "pixels outside the run stay transparent");
}

#[test]
fn availability_matches_the_host_platform() {
    if cfg!(windows) {
        // A device may be missing on a headless runner, but the probe must not panic.
        let _ = is_available();
    }
    else {
        assert!(!is_available(), "Direct3D 12 is Windows only");
    }
}
