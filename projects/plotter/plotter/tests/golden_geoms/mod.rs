//! A2 冒烟：point / bar / text / line 的 Scene + CPU/SVG 回归。
//!
//! 逐字节 golden 语料已按「fixtures 归下游」策略迁出 Boost，见
//! `plotter-audit/fixtures/plotter`。本文件只保留仓内自洽断言：场景结构、
//! SVG 关键元素、非空像素输出，以及双跑确定性（同一输入两次渲染必须逐字节一致）。

use plotter::{
    data::ColumnTable,
    grammar::{AestheticExpr, CompileOptions, LayerSpec, Mapping, PlotSpec, compile_plot},
    renderer::{render_rgba8, svg::render_svg},
    view::SceneNodeKind,
};

fn assert_svg_deterministic(scene: &plotter::view::Scene) {
    let first = render_svg(scene).expect("first svg render");
    let second = render_svg(scene).expect("second svg render");
    assert_eq!(first, second, "svg render must be deterministic");
}

fn assert_image_smoke(scene: &plotter::view::Scene) {
    let image = render_rgba8(scene).expect("cpu raster");
    assert!(image.non_white_count() > 0, "cpu raster produced an empty image");
    assert_eq!(image.fingerprint(), render_rgba8(scene).expect("second cpu raster").fingerprint(), "cpu fingerprint must be stable");
}

#[test]
fn golden_point() {
    let table = ColumnTable::new().push_float("x", vec![0.0, 1.0, 2.0]).unwrap().push_float("y", vec![1.0, 2.0, 1.5]).unwrap();
    let plot = PlotSpec::new(table).mapping(Mapping::xy("x", "y")).layer(LayerSpec::geom_point());
    let scene = compile_plot(&plot, CompileOptions::golden()).unwrap();
    assert!(scene.nodes.nodes().iter().any(|n| matches!(n.kind, SceneNodeKind::Points(_))));

    let svg = render_svg(&scene).unwrap();
    assert!(svg.contains("<circle"));
    assert_svg_deterministic(&scene);
    assert_image_smoke(&scene);
}

#[test]
fn golden_bar() {
    let table = ColumnTable::new().push_float("x", vec![0.0, 1.0, 2.0]).unwrap().push_float("y", vec![1.0, 2.0, 1.5]).unwrap();
    let plot = PlotSpec::new(table).mapping(Mapping::xy("x", "y")).layer(LayerSpec::geom_bar());
    let scene = compile_plot(&plot, CompileOptions::golden()).unwrap();
    assert!(scene.nodes.nodes().iter().any(|n| matches!(n.kind, SceneNodeKind::Mesh(_))));

    let svg = render_svg(&scene).unwrap();
    assert!(svg.contains("<polygon"));
    assert_svg_deterministic(&scene);
    assert_image_smoke(&scene);
}

#[test]
fn golden_text() {
    let table = ColumnTable::new()
        .push_float("x", vec![0.0, 1.0, 2.0])
        .unwrap()
        .push_float("y", vec![1.0, 2.0, 1.5])
        .unwrap()
        .push_string("lab", vec!["a".into(), "b".into(), "c".into()])
        .unwrap();
    let mut mapping = Mapping::xy("x", "y");
    mapping.label = Some(AestheticExpr::column("lab"));
    let plot = PlotSpec::new(table).mapping(mapping).layer(LayerSpec::geom_text());
    let scene = compile_plot(&plot, CompileOptions::golden()).unwrap();
    assert!(scene.nodes.nodes().iter().any(|n| matches!(n.kind, SceneNodeKind::Text(_))));

    let svg = render_svg(&scene).unwrap();
    assert!(svg.contains("<text"));
    assert!(svg.contains(">a</text>"));
    assert_svg_deterministic(&scene);
    assert_image_smoke(&scene);
}

#[test]
fn golden_line_still_works() {
    let table = ColumnTable::new().push_float("x", vec![0.0, 1.0, 2.0]).unwrap().push_float("y", vec![0.0, 1.0, 0.5]).unwrap();
    let plot = PlotSpec::new(table).mapping(Mapping::xy("x", "y")).layer(LayerSpec::geom_line());
    let scene = compile_plot(&plot, CompileOptions::golden()).unwrap();
    let svg = render_svg(&scene).unwrap();
    assert!(!svg.is_empty(), "line svg must not be empty");
    assert_svg_deterministic(&scene);
    assert_image_smoke(&scene);
}
