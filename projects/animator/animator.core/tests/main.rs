//! 时间轴状态、过渡、保持段与错误定位的断言。

use animator_core::{Easing, Program, ProgramError, SceneState, Transition};
use plotter_scene::{DisplayScene, NodeKind, Rect, SceneNode, ShapeStyle, Size, Transform};

/// 一个 200×100 的场景，包含一个带稳定 id 的矩形。
fn scene_with(id: &str, x: f64) -> DisplayScene {
    let node = SceneNode::new(NodeKind::Rect { rect: Rect::new(0.0, 0.0, 10.0, 10.0), style: ShapeStyle::default() })
        .with_id(id)
        .with_transform(Transform::translate(x, 0.0));
    let mut scene = DisplayScene::new(Size::new(200.0, 100.0));
    scene.push(node);
    scene
}

/// 读取场景内某个 id 的横向平移，用来判断插值是否真实发生。
fn x_of(scene: &DisplayScene, id: &str) -> f64 {
    scene
        .nodes
        .iter()
        .find(|node| node.id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("场景里没有 id 为 `{id}` 的节点"))
        .transform
        .e()
}

/// 两状态程序：`from` 秒开始，`from + duration` 处完成，`to` 秒进入下一个状态。
fn program(from: f64, to: f64, duration: f64) -> Program {
    Program {
        states: vec![
            SceneState { at: from, scene: scene_with("bar", 0.0) },
            SceneState { at: to, scene: scene_with("bar", 100.0) },
        ],
        transitions: vec![Transition { duration, ease: Easing::Linear, ..Transition::default() }],
    }
}

#[test]
fn interpolates_inside_a_transition() {
    let compiled = program(0.0, 2.0, 1.0).compile().expect("程序合法");
    assert_eq!(compiled.duration_seconds(), 2.0);
    assert_eq!(x_of(&compiled.sample(0.0), "bar"), 0.0);
    assert_eq!(x_of(&compiled.sample(0.5), "bar"), 50.0);
    assert_eq!(x_of(&compiled.sample(1.0), "bar"), 100.0);
}

#[test]
fn holds_after_the_transition_until_the_next_state() {
    let compiled = program(0.0, 2.0, 1.0).compile().expect("程序合法");
    // 过渡只占前 1 秒，后面 1 秒保持下一个状态的画面。
    assert_eq!(compiled.sample(1.5), scene_with("bar", 100.0));
}

#[test]
fn holds_before_the_first_state() {
    let compiled = program(1.0, 2.0, 0.5).compile().expect("程序合法");
    assert_eq!(compiled.sample(0.0), scene_with("bar", 0.0));
    assert_eq!(compiled.sample(0.9), scene_with("bar", 0.0));
    assert_eq!(x_of(&compiled.sample(1.25), "bar"), 50.0);
}

#[test]
fn zero_duration_is_an_explicit_cut() {
    let compiled = program(0.0, 2.0, 0.0).compile().expect("程序合法");
    assert_eq!(compiled.sample(0.1), scene_with("bar", 100.0));
}

#[test]
fn clamps_after_the_last_state() {
    let compiled = program(0.0, 2.0, 1.0).compile().expect("程序合法");
    assert_eq!(compiled.sample(1_000.0), scene_with("bar", 100.0));
    assert_eq!(compiled.sample(f64::NAN), scene_with("bar", 0.0));
}

#[test]
fn frame_sampling_matches_seconds() {
    let compiled = program(0.0, 2.0, 1.0).compile().expect("程序合法");
    assert_eq!(compiled.sample_frame(30, 15), Some(compiled.sample(0.5)));
    assert_eq!(compiled.sample_frame(0, 1), None);
    assert_eq!(compiled.frame_count(0), None);
}

/// A1：同一时间轴在不同帧率下，帧号到秒的映射可以逐项复算，且帧数覆盖全长。
#[test]
fn frame_mapping_is_reproducible_across_frame_rates() {
    let compiled = program(0.0, 3.0, 1.0).compile().expect("程序合法");
    for fps in [24, 30, 60] {
        let count = compiled.frame_count(fps).expect("fps 非零");
        assert!(
            f64::from(count) / f64::from(fps) >= compiled.duration_seconds(),
            "{fps} fps 的 {count} 帧必须覆盖 {} 秒",
            compiled.duration_seconds()
        );
        // 每个帧号都能用秒复算出同一场景。
        for frame in [0, count / 3, count / 2, count - 1] {
            assert_eq!(
                compiled.sample_frame(fps, frame),
                Some(compiled.sample(f64::from(frame) / f64::from(fps))),
                "{fps} fps frame {frame}"
            );
        }
    }
}

#[test]
fn sampling_is_deterministic() {
    let program = program(0.0, 3.0, 1.2);
    let first = program.compile().expect("程序合法");
    let second = program.compile().expect("程序合法");
    for time in [0.0, 0.3, 1.2, 1.8, 3.0] {
        assert_eq!(first.sample(time), second.sample(time), "时间 {time}");
    }
}

#[test]
fn rejects_invalid_programs() {
    let empty = Program { states: vec![], transitions: vec![] };
    assert_eq!(empty.compile().unwrap_err(), ProgramError::NoStates);

    let mismatched = Program { states: program(0.0, 2.0, 1.0).states, transitions: vec![] };
    assert_eq!(mismatched.compile().unwrap_err(), ProgramError::TransitionCount { states: 2, transitions: 0 });

    let unordered = Program {
        states: vec![
            SceneState { at: 1.0, scene: scene_with("bar", 0.0) },
            SceneState { at: 1.0, scene: scene_with("bar", 100.0) },
        ],
        transitions: vec![Transition { duration: 0.0, ..Transition::default() }],
    };
    assert_eq!(unordered.compile().unwrap_err(), ProgramError::TimeNotAscending { index: 1 });

    let negative = Program {
        states: program(0.0, 2.0, 1.0).states,
        transitions: vec![Transition { duration: -0.5, ..Transition::default() }],
    };
    assert_eq!(negative.compile().unwrap_err(), ProgramError::NegativeDuration { index: 0 });

    let too_long = Program {
        states: program(0.0, 2.0, 1.0).states,
        transitions: vec![Transition { duration: 3.0, ..Transition::default() }],
    };
    assert_eq!(
        too_long.compile().unwrap_err(),
        ProgramError::TransitionTooLong { index: 0, duration: 3.0, gap: 2.0 }
    );
}
