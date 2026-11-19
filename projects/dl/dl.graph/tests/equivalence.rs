//! 图执行与 eager 执行必须给出相同结果，checkpoint 恢复后也一样。

use dl_graph::{Bindings, GraphBuilder, NodeId};
use dl_model;
use dl_nn::{Module, mlp, seeded_random};
use dl_tensor::Tensor;

fn mlp_graph() -> dl_graph::Graph {
    let mut builder = GraphBuilder::new();
    let input = builder.input("x", [4, 2]).unwrap();
    let weight0 = builder.parameter("0.weight", [6, 2]).unwrap();
    let bias0 = builder.parameter("0.bias", [6]).unwrap();
    let weight1 = builder.parameter("2.weight", [1, 6]).unwrap();
    let bias1 = builder.parameter("2.bias", [1]).unwrap();

    let hidden = linear(&mut builder, input, weight0, bias0);
    let hidden = builder.relu(hidden).unwrap();
    let output = linear(&mut builder, hidden, weight1, bias1);
    builder.finish(vec![output]).unwrap()
}

fn linear(builder: &mut GraphBuilder, input: NodeId, weight: NodeId, bias: NodeId) -> NodeId {
    let transposed = builder.transpose(weight, 0, 1).unwrap();
    let product = builder.matmul(input, transposed).unwrap();
    builder.add(product, bias).unwrap()
}

fn bindings(module: &dyn Module, input: Tensor) -> Bindings {
    let mut bindings = Bindings::new();
    bindings.insert_input("x", input);
    for (name, parameter) in module.named_parameters() {
        bindings.insert_parameter(name, parameter.value());
    }
    bindings
}

fn assert_close(actual: &[f32], expected: &[f32]) {
    assert_eq!(actual.len(), expected.len());
    for (position, (left, right)) in actual.iter().zip(expected).enumerate() {
        assert!((left - right).abs() <= 1e-5, "element {position} differs: {left} vs {right}");
    }
}

fn sample() -> Tensor {
    Tensor::from_vec(vec![0.25, -1.5, 2.0, 0.5, -0.75, 1.25, 3.0, -2.0], [4, 2]).unwrap()
}

#[test]
fn graph_matches_eager_forward() {
    let mut random = seeded_random(20261119);
    let model = mlp(&[2, 6, 1], &mut random).unwrap();
    let graph = mlp_graph();
    let input = sample();

    let eager = model.predict(&input).unwrap();
    let graph_output = graph.run(&bindings(&model, input)).unwrap();

    assert_eq!(graph_output.len(), 1);
    assert_eq!(graph_output[0].shape(), eager.shape());
    assert_close(&graph_output[0].to_vec(), &eager.to_vec());
}

#[test]
fn graph_parameters_match_module_names() {
    let mut random = seeded_random(7);
    let model = mlp(&[2, 6, 1], &mut random).unwrap();
    let graph = mlp_graph();

    let graph_names: Vec<String> = graph.parameters().map(|(name, _)| name.to_owned()).collect();
    let module_names: Vec<String> = model.named_parameters().into_iter().map(|(name, _)| name).collect();
    assert_eq!(graph_names, module_names);
}

#[test]
fn checkpoint_restore_then_graph_inference() {
    let mut random = seeded_random(23);
    let source = mlp(&[2, 6, 1], &mut random).unwrap();
    let path = std::env::temp_dir().join(format!("boost-dl-graph-{}-{}.safetensors", std::process::id(), line_id()));
    dl_model::save(&source, &path).unwrap();

    let mut other_random = seeded_random(99);
    let restored = mlp(&[2, 6, 1], &mut other_random).unwrap();
    dl_model::load(&restored, &path).unwrap();

    let graph = mlp_graph();
    let input = sample();
    let expected = source.predict(&input).unwrap();
    let graph_output = graph.run(&bindings(&restored, input)).unwrap();
    assert_close(&graph_output[0].to_vec(), &expected.to_vec());

    std::fs::remove_file(&path).unwrap();
}

/// 用单调计数避免同一进程内重复测试时文件名冲突。
fn line_id() -> u128 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed) as u128
}
