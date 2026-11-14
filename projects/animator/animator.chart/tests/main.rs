//! 图表数据时间轴：状态编译、取样差异与错误定位。

use animator_chart::{ChartState, ChartTimeline, ChartTimelineError};
use plotter_chart::{
    ChannelEncoding, ChartOptions, ChartSpec, DataColumn, DataSnapshot, FieldKind, FieldRef, Mark, Value,
};
use plotter_scene::DisplayScene;

/// 两栏柱状图：`cat` 是分类，`value` 是高度。
fn spec() -> ChartSpec {
    let mut spec = ChartSpec::new(Mark::Bar);
    spec.fields = vec![FieldRef::new("cat", FieldKind::Nominal), FieldRef::new("value", FieldKind::Quantitative)];
    spec.encoding.x = Some(ChannelEncoding::field("cat", FieldKind::Nominal));
    spec.encoding.y = Some(ChannelEncoding::field("value", FieldKind::Quantitative));
    spec
}

/// 两份观察，只有第二列数值不同。
fn data(values: [f64; 2]) -> DataSnapshot {
    DataSnapshot::new(vec![
        DataColumn::new(
            "cat",
            FieldKind::Nominal,
            vec![Value::Text("a".to_string()), Value::Text("b".to_string())],
        ),
        DataColumn::new("value", FieldKind::Quantitative, values.iter().copied().map(Value::Number).collect()),
    ])
}

/// 找到 id 为 `chart-bar-1` 的节点横向平移；柱高变化体现为 transform 差异。
fn bar_x(scene: &DisplayScene, id: &str) -> f64 {
    scene
        .nodes
        .iter()
        .find(|node| node.id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("场景里没有 id 为 `{id}` 的节点"))
        .transform
        .e()
}

#[test]
fn empty_data_states_are_rejected() {
    let timeline = ChartTimeline::new(spec(), ChartOptions::default(), vec![]);
    assert!(matches!(timeline.program(), Err(ChartTimelineError::NoStates)));
}

#[test]
fn transition_count_must_match_states() {
    let mut timeline = ChartTimeline::new(
        spec(),
        ChartOptions::default(),
        vec![
            ChartState { at: 0.0, data: data([1.0, 2.0]) },
            ChartState { at: 2.0, data: data([3.0, 4.0]) },
        ],
    );
    timeline.transitions = vec![];
    // 空过渡是允许的（走缺省），显式给错数量才拒绝。
    assert!(timeline.program().is_ok());
    timeline.transitions = vec![Default::default(), Default::default()];
    assert!(matches!(timeline.program(), Err(ChartTimelineError::TransitionCount { .. })));
}

/// 数据变化必须真的改变画面：起始、结束与中间帧各不相同。
#[test]
fn data_states_change_the_sampled_scene() {
    let timeline = ChartTimeline::new(
        spec(),
        ChartOptions::default(),
        vec![
            ChartState { at: 0.0, data: data([1.0, 2.0]) },
            ChartState { at: 2.0, data: data([1.0, 6.0]) },
        ],
    );
    let compiled = timeline.program().expect("程序合法").compile().expect("时间轴合法");
    let start = compiled.sample(0.0);
    let middle = compiled.sample(0.2);
    let end = compiled.sample(2.0);
    assert_ne!(start, end, "数据状态不同，首尾画面必须不同");
    assert_ne!(middle, start, "过渡中必须出现中间状态");
    assert_ne!(middle, end, "过渡中不能直接跳到终点");
    // 柱 0 没变，柱 1 变了：只有第二个 id 的位置需要移动。
    assert_eq!(bar_x(&start, "chart-bar-0"), bar_x(&end, "chart-bar-0"));
}

/// 同一输入重复编译，任意时刻取样完全相同。
#[test]
fn compilation_is_deterministic() {
    let build = || {
        ChartTimeline::new(
            spec(),
            ChartOptions::default(),
            vec![
                ChartState { at: 0.0, data: data([1.0, 2.0]) },
                ChartState { at: 1.5, data: data([2.0, 5.0]) },
            ],
        )
        .program()
        .expect("程序合法")
        .compile()
        .expect("时间轴合法")
    };
    let first = build();
    let second = build();
    for time in [0.0, 0.25, 0.75, 1.5] {
        assert_eq!(first.sample(time), second.sample(time), "时间 {time}");
    }
    assert_eq!(first.frame_count(30), Some(45));
}
