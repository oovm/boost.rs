//! 门面级冒烟：构造、编译并渲染 2D 折线图。

use plotter::{
    data::ColumnTable,
    grammar::{CompileOptions, LayerSpec, Mapping, PlotSpec, compile_plot, validate_plot},
    renderer::{render_rgba8, svg::render_svg},
    view::SceneNodeKind,
};

#[test]
fn facade_validates_line_plot() {
    let table = ColumnTable::new().push_float("x", vec![0.0, 1.0, 2.0]).unwrap().push_float("y", vec![1.0, 2.0, 3.0]).unwrap();
    let plot = PlotSpec::new(table).mapping(Mapping::xy("x", "y")).layer(LayerSpec::geom_line());
    validate_plot(&plot).unwrap();
}

#[test]
fn facade_compiles_line_plot_to_scene() {
    let table = ColumnTable::new().push_float("x", vec![0.0, 1.0, 2.0]).unwrap().push_float("y", vec![1.0, 2.0, 3.0]).unwrap();
    let plot = PlotSpec::new(table).mapping(Mapping::xy("x", "y")).layer(LayerSpec::geom_line());
    let scene = compile_plot(&plot, CompileOptions::default()).unwrap();
    let root = scene.nodes.get(scene.root).unwrap();
    assert!(matches!(root.kind, SceneNodeKind::Group { .. }));
}

#[test]
fn facade_renders_cpu_and_svg() {
    let table = ColumnTable::new().push_float("x", vec![0.0, 1.0, 2.0]).unwrap().push_float("y", vec![1.0, 2.0, 3.0]).unwrap();
    let plot = PlotSpec::new(table).mapping(Mapping::xy("x", "y")).layer(LayerSpec::geom_line());
    let scene = compile_plot(&plot, CompileOptions::default()).unwrap();
    let image = render_rgba8(&scene).unwrap();
    assert!(image.non_white_count() > 0);
    let svg = render_svg(&scene).unwrap();
    assert!(svg.contains("<polyline"));
}

#[cfg(not(feature = "dx12"))]
#[test]
fn facade_reports_accelerated_backend_unavailable() {
    use plotter::renderer::gpu;

    let table = ColumnTable::new().push_float("x", vec![0.0, 1.0, 2.0]).unwrap().push_float("y", vec![1.0, 2.0, 3.0]).unwrap();
    let plot = PlotSpec::new(table).mapping(Mapping::xy("x", "y")).layer(LayerSpec::geom_line());
    let scene = compile_plot(&plot, CompileOptions::default()).unwrap();
    assert!(!gpu::is_available());
    assert!(matches!(gpu::render(&scene).unwrap_err(), gpu::GpuError::Unavailable));
}

#[cfg(feature = "dx12")]
#[test]
fn facade_renders_through_the_accelerated_backend_when_present() {
    use plotter::renderer::gpu;

    let table = ColumnTable::new().push_float("x", vec![0.0, 1.0, 2.0]).unwrap().push_float("y", vec![1.0, 2.0, 3.0]).unwrap();
    let plot = PlotSpec::new(table).mapping(Mapping::xy("x", "y")).layer(LayerSpec::geom_line());
    let scene = compile_plot(&plot, CompileOptions::default()).unwrap();
    if !gpu::is_available() {
        eprintln!("skip accelerated facade render: no usable Direct3D 12 device on this host");
        return;
    }
    let image = gpu::render(&scene).expect("a D3D12-capable host should render the scene");
    assert_eq!(image.pixels.len(), (image.width * image.height * 4) as usize);
    assert!(image.pixels.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0), "the accelerated frame must be painted");
}
