//! 火焰图与深渊图：把 folded stacks 直接渲染成 SVG。
//!
//! 不依赖任何外部工具（`inferno`、`flamegraph.pl`、浏览器运行时），也不
//! 依赖绘图库：解析、布局、配色和 SVG 序列化都在本模块内完成。输入是
//! [`crate::profile::Profiler::folded`] 产出的纯文本，因此图与样本可以逐字节
//! 对上。
//!
//! 两种方向共用同一份布局：
//! - [`FlameOrientation::Flame`] 根在底部、子帧向上堆叠，即经典火焰图；
//! - [`FlameOrientation::Icicle`] 根在顶部、子帧向下堆叠，即深渊图/冰柱图。
//!
//! 宽度按包含耗时占比计算，颜色由完整路径哈希决定（同一帧在不同运行里
//! 颜色一致），因此同一份 folded 输入总是得到字节一致的 SVG。

use std::{collections::BTreeMap, fmt::Write as _};

use serde::{Deserialize, Serialize};

use crate::HarnessError;

/// 火焰图方向。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlameOrientation {
    /// 根在底部，子帧向上（经典火焰图）。
    #[default]
    Flame,
    /// 根在顶部，子帧向下（深渊图/冰柱图）。
    Icicle,
}

/// 火焰图布局参数。
#[derive(Debug, Clone, PartialEq)]
pub struct FlameOptions {
    /// SVG 宽度，单位像素。
    pub width: u32,
    /// 每层帧的高度，单位像素。
    pub row_height: u32,
    /// 帧内文字字号，单位像素。
    pub font_size: f64,
    /// 小于该宽度的帧不画标签；帧本身仍然绘制。
    pub min_label_width: f64,
    /// 最大绘制层数；`0` 表示不截断。
    pub max_depth: usize,
    /// 图标题；`None` 表示不画标题。
    pub title: Option<String>,
}

impl Default for FlameOptions {
    fn default() -> Self {
        Self { width: 1200, row_height: 20, font_size: 12.0, min_label_width: 24.0, max_depth: 0, title: None }
    }
}

/// 把 folded stacks 渲染为火焰图或深渊图 SVG。
///
/// 每行格式为 `a;b;c <权重>`，权重是无符号整数（与本 crate 的纳秒口径一致）。
/// 空输入、非法权重、空路径和零总权重都返回错误，不画一张会误导人的空图。
pub fn render(folded: &str, orientation: FlameOrientation, options: &FlameOptions) -> crate::Result<String> {
    let root = parse(folded)?;
    if root.value == 0 {
        return Err(HarnessError::Profile("folded stacks carry no weight".to_owned()));
    }
    let depth = height(&root);
    let drawn_depth = if options.max_depth == 0 { depth } else { depth.min(options.max_depth) };
    let margin = 12.0_f64;
    let title_height = if options.title.is_some() { options.font_size * 2.0 } else { 0.0 };
    let plot_width = f64::from(options.width).max(64.0) - margin * 2.0;
    let plot_height = drawn_depth.max(1) as f64 * f64::from(options.row_height);
    let height = plot_height + title_height + margin * 2.0;
    let plot_top = margin + title_height;
    let scale = plot_width / root.value as f64;

    let mut frames = Vec::new();
    layout(&root, 0.0, 0, scale, options, &mut frames);

    let mut svg = String::with_capacity(256 + frames.len() * 256);
    let _ = write!(
        svg,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\" role=\"img\">",
        options.width, height, options.width, height
    );
    if let Some(title) = &options.title {
        let _ = write!(
            svg,
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"{:.1}\" fill=\"#111827\">{}</text>",
            f64::from(options.width) / 2.0,
            margin + options.font_size,
            options.font_size * 1.2,
            escape(title)
        );
    }
    let _ = write!(svg, "<rect x=\"0\" y=\"0\" width=\"{}\" height=\"{}\" fill=\"#ffffff\"/>", options.width, height);

    for frame in &frames {
        let y = match orientation {
            FlameOrientation::Flame => plot_top + plot_height - (frame.depth + 1) as f64 * f64::from(options.row_height),
            FlameOrientation::Icicle => plot_top + frame.depth as f64 * f64::from(options.row_height),
        };
        let percent = frame.value as f64 * 100.0 / root.value as f64;
        let _ = write!(
            svg,
            "<g class=\"flame-frame\" data-depth=\"{}\" data-value=\"{}\"><title>{} ({} ns, {:.1}%)</title>",
            frame.depth,
            frame.value,
            escape(&frame.name),
            frame.value,
            percent
        );
        let _ = write!(
            svg,
            "<rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{}\" fill=\"{}\" stroke=\"#ffffff\" stroke-width=\"0.5\"/>",
            margin + frame.x,
            y,
            frame.width,
            options.row_height,
            color_for(&frame.name, orientation)
        );
        if frame.width >= options.min_label_width {
            let label = truncate(&frame.name, frame.width - 4.0, options.font_size);
            let _ = write!(
                svg,
                "<text x=\"{:.2}\" y=\"{:.2}\" font-family=\"sans-serif\" font-size=\"{:.1}\" fill=\"#111827\">{}</text>",
                margin + frame.x + 2.0,
                y + (f64::from(options.row_height) + options.font_size) / 2.0 - 2.0,
                options.font_size,
                escape(&label)
            );
        }
        svg.push_str("</g>");
    }
    svg.push_str("</svg>");
    Ok(svg)
}

#[derive(Debug, Default)]
struct Frame {
    name: String,
    value: u64,
    children: BTreeMap<String, Frame>,
}

impl Frame {
    fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), value: 0, children: BTreeMap::new() }
    }
}

#[derive(Debug)]
struct DrawnFrame {
    name: String,
    value: u64,
    depth: usize,
    x: f64,
    width: f64,
}

fn parse(folded: &str) -> crate::Result<Frame> {
    let mut root = Frame::new("");
    let mut seen = false;
    for (index, line) in folded.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((path, weight)) = line.rsplit_once(' ')
        else {
            return Err(HarnessError::Profile(format!("folded line {} has no weight: {line:?}", index + 1)));
        };
        let weight: u64 =
            weight.parse().map_err(|_| HarnessError::Profile(format!("folded line {} has a non-integer weight: {weight:?}", index + 1)))?;
        if weight == 0 {
            return Err(HarnessError::Profile(format!("folded line {} has zero weight", index + 1)));
        }
        if path.is_empty() {
            return Err(HarnessError::Profile(format!("folded line {} has an empty path", index + 1)));
        }
        seen = true;
        root.value = root.value.saturating_add(weight);
        let mut node = &mut root;
        for name in path.split(';') {
            if name.is_empty() {
                return Err(HarnessError::Profile(format!("folded line {} has an empty frame name", index + 1)));
            }
            node = node.children.entry(name.to_owned()).or_insert_with(|| Frame::new(name));
            node.value = node.value.saturating_add(weight);
        }
    }
    if !seen {
        return Err(HarnessError::Profile("folded stacks are empty".to_owned()));
    }
    Ok(root)
}

fn height(node: &Frame) -> usize {
    node.children.values().map(|child| 1 + height(child)).max().unwrap_or(0)
}

fn layout(node: &Frame, x: f64, depth: usize, scale: f64, options: &FlameOptions, out: &mut Vec<DrawnFrame>) {
    let mut cursor = x;
    for child in node.children.values() {
        let width = child.value as f64 * scale;
        if options.max_depth == 0 || depth < options.max_depth {
            out.push(DrawnFrame { name: child.name.clone(), value: child.value, depth, x: cursor, width });
        }
        layout(child, cursor, depth + 1, scale, options, out);
        cursor += width;
    }
}

/// 同一路径在任何运行里得到同一种颜色。
fn color_for(path: &str, orientation: FlameOrientation) -> &'static str {
    const FLAME: [&str; 8] = ["#f59e0b", "#ef4444", "#f97316", "#eab308", "#fb7185", "#d97706", "#f43f5e", "#fbbf24"];
    const ICICLE: [&str; 8] = ["#3b82f6", "#06b6d4", "#8b5cf6", "#10b981", "#6366f1", "#0ea5e9", "#14b8a6", "#a855f7"];
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let palette = match orientation {
        FlameOrientation::Flame => &FLAME,
        FlameOrientation::Icicle => &ICICLE,
    };
    palette[(hash as usize) % palette.len()]
}

/// 按近似字符宽度截断，保证标签不越过帧边界。
fn truncate(label: &str, width: f64, font_size: f64) -> String {
    let capacity = (width / (font_size * 0.6)).floor().max(0.0) as usize;
    if label.chars().count() <= capacity {
        return label.to_owned();
    }
    if capacity == 0 {
        return String::new();
    }
    let mut text: String = label.chars().take(capacity.saturating_sub(1)).collect();
    text.push('…');
    text
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOLDED: &str = "load;parse 300\nload;apply 700\nother 1000\n";

    #[test]
    fn both_orientations_draw_every_frame() {
        let flame = render(FOLDED, FlameOrientation::Flame, &FlameOptions::default()).expect("flame svg");
        let icicle = render(FOLDED, FlameOrientation::Icicle, &FlameOptions::default()).expect("icicle svg");
        assert!(flame.starts_with("<svg") && flame.ends_with("</svg>"));
        // load / parse / apply / other 四个唯一帧，两条子路径合并到 load。
        assert_eq!(flame.matches("class=\"flame-frame\"").count(), 4);
        assert_eq!(icicle.matches("class=\"flame-frame\"").count(), 4);
        assert!(flame.contains("data-value=\"1000\""));
        assert_ne!(flame, icicle, "方向必须改变布局");
    }

    #[test]
    fn widths_follow_inclusive_weight() {
        let svg = render(FOLDED, FlameOrientation::Icicle, &FlameOptions::default()).expect("svg");
        // load 占一半，other 占一半；画布 1200 - 2*12 = 1176。
        assert!(svg.contains("width=\"588.00\""), "{svg}");
    }

    #[test]
    fn labels_and_titles_are_xml_escaped() {
        let svg =
            render("a;b&c<d 10", FlameOrientation::Flame, &FlameOptions { title: Some("t <x>".into()), ..Default::default() }).expect("svg");
        assert!(svg.contains("b&amp;c&lt;d"), "{svg}");
        assert!(svg.contains("t &lt;x&gt;"));
    }

    #[test]
    fn rendering_is_deterministic() {
        let first = render(FOLDED, FlameOrientation::Flame, &FlameOptions::default()).expect("first");
        let second = render(FOLDED, FlameOrientation::Flame, &FlameOptions::default()).expect("second");
        assert_eq!(first, second);
    }

    #[test]
    fn malformed_input_is_refused() {
        assert!(render("", FlameOrientation::Flame, &FlameOptions::default()).is_err());
        assert!(render("a;b 1\nnot-a-sample\n", FlameOrientation::Flame, &FlameOptions::default()).is_err());
        assert!(render("a;b nope\n", FlameOrientation::Flame, &FlameOptions::default()).is_err());
        assert!(render("a;;b 1\n", FlameOrientation::Flame, &FlameOptions::default()).is_err());
        assert!(render("a 0\n", FlameOrientation::Flame, &FlameOptions::default()).is_err());
    }

    #[test]
    fn max_depth_stops_drawing_deeper_frames() {
        let svg = render(FOLDED, FlameOrientation::Flame, &FlameOptions { max_depth: 1, ..Default::default() }).expect("svg");
        assert!(!svg.contains("data-depth=\"1\""), "deeper frames are not drawn");
        assert!(svg.contains("data-depth=\"0\""));
    }
}
