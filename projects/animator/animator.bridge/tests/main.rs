//! 帧计划、单帧 SVG、写出文件与错误定位。

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
};

use animator_bridge::{BridgeError, frame_svg, plan_frames, write_frames};
use animator_core::{Program, SceneState};
use plotter_scene::{Color, DisplayScene, NodeKind, Rect, SceneNode, ShapeStyle, Size};

/// 一个带稳定 id 的矩形场景；`x` 变化模拟图元平移。
fn scene(width: f64, height: f64, x: f64) -> DisplayScene {
    let mut scene = DisplayScene::new(Size { width, height });
    let mut node =
        SceneNode::new(NodeKind::Rect { rect: Rect { x, y: 10.0, width: 20.0, height: 30.0 }, style: ShapeStyle::filled(Color::BLACK) });
    node.id = Some("mark".to_string());
    scene.push(node);
    scene
}

/// 两个状态、一秒长的时间轴。
fn program() -> Program {
    Program::new(vec![SceneState { at: 0.0, scene: scene(320.0, 180.0, 0.0) }, SceneState { at: 1.0, scene: scene(320.0, 180.0, 50.0) }])
}

/// 每个用例独享的临时目录，不依赖外部 crate。
fn temp_dir() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("animator-bridge-test-{}-{unique}", std::process::id()))
}

#[test]
fn frame_plan_matches_frame_count() {
    let compiled = program().compile().expect("时间轴合法");
    let plan = plan_frames(&compiled, 10).expect("帧率合法");
    assert_eq!(plan, animator_bridge::FramePlan { fps: 10, frames: 10 });
    assert_eq!(Some(plan.frames), compiled.frame_count(10));
}

#[test]
fn zero_fps_is_rejected() {
    let compiled = program().compile().expect("时间轴合法");
    assert!(matches!(plan_frames(&compiled, 0), Err(BridgeError::FpsZero)));
    assert!(matches!(frame_svg(&compiled, 0, 0), Err(BridgeError::FpsZero)));
}

#[test]
fn out_of_range_frame_is_rejected() {
    let compiled = program().compile().expect("时间轴合法");
    assert!(matches!(frame_svg(&compiled, 10, 10), Err(BridgeError::FrameOutOfRange { frame: 10, frames: 10 })));
}

/// 桥接帧与同一时刻直接序列化的静态场景逐字节一致（A3）。
#[test]
fn frame_matches_the_static_scene() {
    let compiled = program().compile().expect("时间轴合法");
    for frame in [0_u32, 3, 5, 9] {
        let expected = compiled.sample(f64::from(frame) / 10.0).to_svg().expect("场景合法");
        assert_eq!(frame_svg(&compiled, 10, frame).expect("桥接成功"), expected, "第 {frame} 帧");
    }
}

#[test]
fn frames_are_written_with_stable_names_and_fingerprints() {
    let compiled = program().compile().expect("时间轴合法");
    let first = temp_dir();
    let second = temp_dir();
    let report = write_frames(&compiled, 10, &first).expect("写出成功");
    let repeated = write_frames(&compiled, 10, &second).expect("重复写出成功");
    assert_eq!(report, repeated, "相同程序与帧率必须得到相同报告");
    assert_eq!(report.frames, 10);
    assert_eq!(report.fps, 10);
    assert_eq!(report.seconds, 1.0);
    assert_eq!((report.width, report.height), (320.0, 180.0));

    for (frame, expected) in [(0_u32, frame_svg(&compiled, 10, 0).expect("桥接成功")), (9, frame_svg(&compiled, 10, 9).expect("桥接成功"))]
    {
        let written = fs::read_to_string(first.join(format!("frame-{frame:04}.svg"))).expect("文件存在");
        assert_eq!(written, expected, "第 {frame} 帧内容");
        assert_eq!(fs::read_to_string(second.join(format!("frame-{frame:04}.svg"))).expect("文件存在"), expected);
    }
    assert!(!first.join("frame-0010.svg").exists(), "不得多写一帧");

    fs::remove_dir_all(&first).ok();
    fs::remove_dir_all(&second).ok();
}

#[test]
fn canvas_size_must_stay_constant() {
    let program =
        Program::new(vec![SceneState { at: 0.0, scene: scene(320.0, 180.0, 0.0) }, SceneState { at: 0.5, scene: scene(640.0, 180.0, 50.0) }]);
    let compiled = program.compile().expect("时间轴合法");
    let directory = temp_dir();
    let error = write_frames(&compiled, 10, &directory).expect_err("尺寸变化必须失败");
    assert!(matches!(error, BridgeError::SizeChanged { .. }), "实际错误：{error}");
    fs::remove_dir_all(&directory).ok();
}
