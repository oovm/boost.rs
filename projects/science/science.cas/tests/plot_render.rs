//! 1D 采样 → plotter SVG 渲染组装端到端集成测试。

use science_cas::{
    Session,
    plot::{SampleDomain, SamplingPolicy, plot_1d_svg},
    runtime::values::arena::{push_int, push_semantic, push_symbol_name},
};
use science_ir::SemanticOperator;

#[test]
fn square_renders_polyline_svg() {
    let mut session = Session::new();
    let x = push_symbol_name(&mut session, "x");
    let two = push_int(&mut session, 2);
    let expr = push_semantic(&mut session, SemanticOperator::Power, vec![x, two]);
    let svg = plot_1d_svg(&mut session, expr, "x", SampleDomain::new(-1.0, 1.0), SamplingPolicy::samples(32)).expect("svg");
    assert!(svg.contains("<svg"), "{svg}");
    assert!(svg.contains("<polyline") || svg.contains("<path"), "{svg}");
}
