//! `MoleculeSpec / ReactionSpec` → `DepictionPlan` 的确定性平面描绘编译。
//!
//! 布局规则（v0）：
//!
//! - 环优先：3–8 元简单环按正多边形放置，首环居中；与已放置环共享一条键的稠环
//!   沿该键法向的反侧展开，方向按「离已有原子最远」确定性选择。
//! - 链原子按 BFS 展开，候选方向为父键方向的 ±60°、±120° 与直行，取「离已有原子
//!   最远」者；最优位置与已有原子过近时报 `davinci.depiction.unplaceable`，只是拥挤
//!   时记一条 `davinci.depiction.approximated`。
//! - 键按键级画多重线：双键两条、三键三条、芳香键一条实线加一条内侧虚线。
//! - 带标签的原子在键端留白，文字下垫背景遮住键。

use std::{
    collections::VecDeque,
    f64::consts::{FRAC_PI_2, PI, TAU},
};

use plotter_scene::TextAnchor;
use plotter_types::{Color, Diagnostic, DiagnosticSeverity, Point, Size, codes, diagnostic};

use crate::{
    error::DepictionError,
    plan::{DepictionOptions, DepictionPlan, LabelPlan, LinePlan, Palette},
    spec::{ArrowKind, Atom, BondOrder, Element, MoleculeSpec, ReactionSpec},
};

/// v0 接受的最大环尺寸。
const MAX_RING: usize = 8;
/// 浮点比较容差。
const EPS: f64 = 1e-9;
/// 并行多重键的偏移（键长倍数）。
const DOUBLE_OFFSET: f64 = 0.085;
/// 三键中间两侧的偏移。
const TRIPLE_OFFSET: f64 = 0.135;
/// 芳香键内侧线偏移。
const AROMATIC_OFFSET: f64 = 0.11;
/// 彩色描绘里的加号与反应条件文字色。
const NOTE_GREY: Color = Color::rgb(0x57, 0x60, 0x6A);

/// CPK 元素配色。取值沿用化学信息学通行方案，只把过亮的黄、绿压暗一档
/// 以保证白底可读；表外元素给中性灰。
fn element_color(element: &Element) -> Color {
    match element {
        Element::H => Color::rgb(0xE6, 0xE6, 0xE6),
        Element::B => Color::rgb(0xFF, 0xB5, 0xB5),
        Element::C => Color::rgb(0x40, 0x40, 0x40),
        Element::N => Color::rgb(0x30, 0x50, 0xF8),
        Element::O => Color::rgb(0xE0, 0x1B, 0x1B),
        Element::F => Color::rgb(0x4C, 0xAF, 0x50),
        Element::Si => Color::rgb(0xC0, 0xA4, 0x6E),
        Element::P => Color::rgb(0xFF, 0x80, 0x00),
        Element::S => Color::rgb(0xB8, 0x94, 0x00),
        Element::Cl => Color::rgb(0x1F, 0xA5, 0x3A),
        Element::Br => Color::rgb(0xA6, 0x29, 0x29),
        Element::I => Color::rgb(0x94, 0x00, 0x94),
        Element::Other(_) => Color::rgb(0x6E, 0x77, 0x81),
    }
}

/// 两条线段是否真正相交（共享端点不算）。
fn segments_cross(a: Point, b: Point, c: Point, d: Point) -> bool {
    let cross = |p: Point, q: Point, r: Point| (q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x);
    let d1 = cross(c, d, a);
    let d2 = cross(c, d, b);
    let d3 = cross(a, b, c);
    let d4 = cross(a, b, d);
    ((d1 > EPS) != (d2 > EPS)) && ((d3 > EPS) != (d4 > EPS))
}

/// 键级对应的数值，用于价校验。
fn order_value(order: BondOrder) -> f64 {
    match order {
        BondOrder::Single => 1.0,
        BondOrder::Double => 2.0,
        BondOrder::Triple => 3.0,
        BondOrder::Aromatic => 1.5,
    }
}

/// 校验分子图的局部结构。
fn validate(spec: &MoleculeSpec) -> Result<(), DepictionError> {
    if spec.atoms.is_empty() {
        return Err(DepictionError::unsupported("a molecule needs at least one atom"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for bond in &spec.bonds {
        if bond.from >= spec.atoms.len() || bond.to >= spec.atoms.len() {
            return Err(DepictionError::unsupported(format!(
                "bond endpoint ({}, {}) is outside the {} declared atoms",
                bond.from,
                bond.to,
                spec.atoms.len()
            )));
        }
        if bond.from == bond.to {
            return Err(DepictionError::unsupported(format!("atom {} declares a bond to itself", bond.from)));
        }
        let key = if bond.from < bond.to { (bond.from, bond.to) } else { (bond.to, bond.from) };
        if !seen.insert(key) {
            return Err(DepictionError::unsupported(format!("atoms {} and {} declare more than one bond", key.0, key.1)));
        }
    }
    for (index, atom) in spec.atoms.iter().enumerate() {
        let Some(base) = atom.element.base_valence()
        else {
            continue;
        };
        let explicit = f64::from(atom.hydrogens.unwrap_or(0));
        // 芳香键按 1.0 记入价校验：Kekulé 式里吡啶型 N 的两条环键是「一双一单」，
        // 吡咯型 N / 呋喃 O 的两条环键全是单键，1.5 的均摊记法会把它们都算超。
        // 低估只会放过结构怪但画得出来的输入，高估则把咖啡因、呋喃这类常见分子
        // 整例拒绝，因此校验取低估一侧。几何仍用 1.5 的键级语义。
        let total: f64 = spec
            .bonds
            .iter()
            .filter(|bond| bond.from == index || bond.to == index)
            .map(|bond| if bond.order == BondOrder::Aromatic { 1.0 } else { order_value(bond.order) })
            .sum::<f64>()
            + explicit;
        // 电荷抬高价（铵 N+ 四键、硝基 N+ 四键），硫/磷/卤素允许常见超价形态
        // （亚砜、砜、磷酸酯）。
        let ceiling = f64::from(atom.element.max_valence().max(base as i16 + i16::from(atom.charge)).max(0));
        if total > ceiling + EPS {
            return Err(DepictionError::unsupported(format!(
                "atom {} ({}{}) carries {total} bond order units over the allowed {ceiling}",
                index,
                atom.element.symbol(),
                charge_text(atom)
            )));
        }
    }
    Ok(())
}

/// 原子的电荷文本，供诊断。
fn charge_text(atom: &Atom) -> String {
    match atom.charge {
        0 => String::new(),
        charge if charge > 0 => format!("{charge}+"),
        charge => format!("{}+", -charge),
    }
}

/// 邻接表，邻居按下标升序，便于确定性遍历。
fn adjacency(spec: &MoleculeSpec) -> Vec<Vec<usize>> {
    let mut adj = vec![Vec::new(); spec.atoms.len()];
    for bond in &spec.bonds {
        adj[bond.from].push(bond.to);
        adj[bond.to].push(bond.from);
    }
    for list in &mut adj {
        list.sort_unstable();
        list.dedup();
    }
    adj
}

/// 枚举所有 3..=8 元简单环，按「环小优先、成员字典序」排序去重。
fn find_rings(adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = adj.len();
    let mut rings: Vec<Vec<usize>> = Vec::new();
    let mut seen: std::collections::BTreeSet<Vec<usize>> = std::collections::BTreeSet::new();
    for start in 0..n {
        let mut path = vec![start];
        let mut on_path = vec![false; n];
        on_path[start] = true;
        ring_dfs(start, start, &mut path, &mut on_path, adj, &mut rings, &mut seen);
    }
    rings.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    rings
}

fn ring_dfs(
    start: usize,
    current: usize,
    path: &mut Vec<usize>,
    on_path: &mut [bool],
    adj: &[Vec<usize>],
    rings: &mut Vec<Vec<usize>>,
    seen: &mut std::collections::BTreeSet<Vec<usize>>,
) {
    for &next in &adj[current] {
        if next == start {
            if path.len() >= 3 && path.len() <= MAX_RING {
                let mut key = path.clone();
                key.sort_unstable();
                if seen.insert(key) {
                    rings.push(path.clone());
                }
            }
            continue;
        }
        if next < start || on_path[next] || path.len() >= MAX_RING {
            continue;
        }
        on_path[next] = true;
        path.push(next);
        ring_dfs(start, next, path, on_path, adj, rings, seen);
        path.pop();
        on_path[next] = false;
    }
}

/// 正多边形半径（给定环尺寸与键长）。
fn polygon_radius(size: usize, length: f64) -> f64 {
    length / (2.0 * (PI / size as f64).sin())
}

/// 到所有已放置原子的最小距离。
fn clearance(point: Point, placed: &[Option<Point>], skip: &[usize]) -> f64 {
    placed
        .iter()
        .enumerate()
        .filter(|(index, position)| position.is_some() && !skip.contains(index))
        .map(|(_, position)| point.distance(position.expect("checked")))
        .fold(f64::INFINITY, f64::min)
}

/// 单位垂直向量。
fn perpendicular(from: Point, to: Point) -> Point {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let len = dx.hypot(dy).max(EPS);
    Point::new(-dy / len, dx / len)
}

/// 把角度规整到 `(-pi, pi]`。
fn wrap(angle: f64) -> f64 {
    let mut value = angle;
    while value <= -PI {
        value += TAU;
    }
    while value > PI {
        value -= TAU;
    }
    value
}

/// 计算分子内全部原子的平面坐标，返回位置与用到的环。
fn layout(
    spec: &MoleculeSpec,
    length: f64,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(Vec<Point>, Vec<Vec<usize>>), DepictionError> {
    let adj = adjacency(spec);
    let mut placed: Vec<Option<Point>> = vec![None; spec.atoms.len()];
    let mut parent: Vec<Option<usize>> = vec![None; spec.atoms.len()];
    // 上一个原子向本原子拐的方向（+1 / -1）。子原子取反，链因此成为 zigzag
    // 而不是一条直线。
    let mut turn: Vec<f64> = vec![1.0; spec.atoms.len()];
    let rings = find_rings(&adj);

    // 1. 环优先。
    for ring in &rings {
        let size = ring.len();
        let radius = polygon_radius(size, length);
        let filled: Vec<usize> = ring.iter().copied().filter(|atom| placed[*atom].is_some()).collect();
        if filled.is_empty() {
            for (index, &atom) in ring.iter().enumerate() {
                let angle = -FRAC_PI_2 + TAU * index as f64 / size as f64;
                placed[atom] = Some(Point::new(radius * angle.cos(), radius * angle.sin()));
            }
            // 已有几何在别处时（联苯、尼古丁这类「环-单键-环」），把新环锚到
            // 与它成键的已放置原子旁：旋转多边形让连接顶点正对锚点，环心落在
            // 「锚点沿背离质心方向键长 + 半径」处，环整体朝外侧展开。
            if let Some((ring_atom, anchor_atom)) = ring.iter().copied().find_map(|atom| {
                adj[atom]
                    .iter()
                    .copied()
                    .find(|neighbour| placed[*neighbour].is_some() && !ring.contains(neighbour))
                    .map(|neighbour| (atom, neighbour))
            }) {
                let anchor = placed[anchor_atom].expect("placed");
                let count = placed.iter().flatten().count() as f64;
                let centroid = Point::new(
                    placed.iter().flatten().map(|p| p.x).sum::<f64>() / count,
                    placed.iter().flatten().map(|p| p.y).sum::<f64>() / count,
                );
                let mut dir = Point::new(centroid.x - anchor.x, centroid.y - anchor.y);
                if dir.x.hypot(dir.y) < EPS {
                    dir = Point::new(-1.0, 0.0);
                }
                let len = dir.x.hypot(dir.y);
                dir = Point::new(-dir.x / len, -dir.y / len);
                let center = Point::new(anchor.x + (length + radius) * dir.x, anchor.y + (length + radius) * dir.y);
                let point = placed[ring_atom].expect("placed");
                // 连接顶点绕环心转到正对锚点的一侧（偏移方向为 -dir）。
                let delta = (-dir.y).atan2(-dir.x) - point.y.atan2(point.x);
                let (sin_d, cos_d) = delta.sin_cos();
                for &atom in ring {
                    let old = placed[atom].expect("placed");
                    placed[atom] = Some(Point::new(
                        center.x + old.x * cos_d - old.y * sin_d,
                        center.y + old.x * sin_d + old.y * cos_d,
                    ));
                }
            }
            continue;
        }
        if filled.len() != 2 {
            diagnostics.push(named_loss(&format!(
                "a {size}-membered ring shares {} atoms with placed rings; the bridged or spiro geometry uses chain placement",
                filled.len()
            )));
            continue;
        }
        let (a, b) = (filled[0], filled[1]);
        let (pa, pb) = (placed[a].expect("placed"), placed[b].expect("placed"));
        if (pa.distance(pb) - length).abs() > 0.3 * length {
            diagnostics.push(named_loss(&format!(
                "a {size}-membered ring shares two atoms that are not bonded; the fused geometry uses chain placement"
            )));
            continue;
        }

        let mid = Point::new((pa.x + pb.x) * 0.5, (pa.y + pb.y) * 0.5);
        let offset = (radius * radius - (length * 0.5).powi(2)).max(0.0).sqrt();
        let normal = perpendicular(pa, pb);
        let rest: Vec<usize> = ring.iter().copied().filter(|atom| *atom != a && *atom != b).collect();
        if rest.len() != size - 2 {
            continue;
        }

        // 两种圆心 × 环表两个绕行方向 × 角度两个增减方向里，取
        // 「中间原子离已有原子最远」的组合。共享边 (a,b) 在环表里可能
        // 相邻于头部或首尾，且环表绕向与角度增减方向彼此独立，三者必须
        // 分开试，否则 5+6 稠环（吲哚、咖啡因）会配不出朝向。
        let ia = ring.iter().position(|atom| *atom == a).expect("in ring");
        let mut best: Option<(f64, Vec<(usize, Point)>)> = None;
        for sign in [1.0, -1.0] {
            let center = Point::new(mid.x + normal.x * offset * sign, mid.y + normal.y * offset * sign);
            let theta_a = (pa.y - center.y).atan2(pa.x - center.x);
            let theta_b = (pb.y - center.y).atan2(pb.x - center.x);
            let step = TAU / size as f64;
            for walk in [1isize, -1isize] {
                for angle_sign in [1.0, -1.0] {
                    let k_b = (1..size).find(|k| ring[(ia as isize + walk * *k as isize).rem_euclid(size as isize) as usize] == b);
                    let Some(k_b) = k_b else { continue };
                    let expected = theta_a + angle_sign * step * k_b as f64;
                    if (wrap(theta_b - expected)).abs() > step * 0.25 {
                        continue;
                    }
                    let mut candidates = Vec::with_capacity(size - 2);
                    let mut score = f64::INFINITY;
                    for k in 1..size as isize {
                        if k == k_b as isize {
                            continue;
                        }
                        let atom = ring[(ia as isize + walk * k).rem_euclid(size as isize) as usize];
                        let angle = theta_a + angle_sign * step * k as f64;
                        let point = Point::new(center.x + radius * angle.cos(), center.y + radius * angle.sin());
                        score = score.min(clearance(point, &placed, ring));
                        candidates.push((atom, point));
                    }
                    if best.as_ref().is_none_or(|(best_score, _)| score > *best_score) {
                        best = Some((score, candidates));
                    }
                }
            }
        }
        match best {
            Some((score, candidates)) => {
                if score < length * 0.5 {
                    // 朝向都对但空间不够：交给链式放置兜底（带避碰与防交叉），
                    // 比整例拒绝更有用，并把这次降级记成命名损失。
                    diagnostics.push(named_loss(&format!(
                        "a {size}-membered fused ring has no room beside the placed rings; its atoms use chain placement"
                    )));
                    continue;
                }
                if score < length {
                    diagnostics.push(named_loss(&format!("a {size}-membered fused ring is crowded against its neighbour")));
                }
                for (atom, point) in candidates {
                    placed[atom] = Some(point);
                }
            }
            None => diagnostics.push(named_loss(&format!("a {size}-membered fused ring has no matching polygon orientation"))),
        }
    }

    // 2. 链原子 BFS；未连通的组分逐个另起一块。
    let mut cursor_x = placed.iter().flatten().map(|point| point.x).fold(f64::NEG_INFINITY, f64::max);
    loop {
        let anchor = match placed.iter().position(Option::is_none) {
            Some(index) => index,
            None => break,
        };
        if placed.iter().all(Option::is_none) {
            placed[anchor] = Some(Point::ZERO);
        }
        else if !touches_placed(anchor, &placed, &adj) {
            // 整个连通分量都没有已放置原子（如盐的另一半），才另起一块。
            // 只看直接邻居会把「环外取代基」误判成独立片段，把整条支链
            // 摆到画布另一侧并留下一条超长键。
            cursor_x += length * 2.5;
            placed[anchor] = Some(Point::new(cursor_x, 0.0));
            diagnostics.push(named_loss("a second disconnected fragment is placed to the right of the first"));
        }

        // 以全部已放置原子为种子展开 BFS：这样锚点所在的连通分量无论从哪一端
        // 接入已放置几何都会被完整摆出，外层循环也必然前进。
        let mut queue: VecDeque<usize> =
            placed.iter().enumerate().filter_map(|(index, point)| point.map(|_| index)).collect();
        while let Some(current) = queue.pop_front() {
            let Some(current_position) = placed[current]
            else {
                continue;
            };
            for &neighbour in &adj[current] {
                if placed[neighbour].is_some() {
                    continue;
                }
                // 参考方向取「已放置质心 → 当前原子」的径向：链沿径向外扩，
                // 天然不会自绕成假环；径向退化（首原子或恰在质心）时退回
                // 上一根键的方向，再退化为 +x。
                let count = placed.iter().flatten().count() as f64;
                let centroid = Point::new(
                    placed.iter().flatten().map(|point| point.x).sum::<f64>() / count,
                    placed.iter().flatten().map(|point| point.y).sum::<f64>() / count,
                );
                let radial = Point::new(current_position.x - centroid.x, current_position.y - centroid.y);
                let base = if radial.x.hypot(radial.y) > 0.2 * length {
                    radial.y.atan2(radial.x)
                }
                else {
                    match parent[current] {
                        Some(previous) => {
                            let previous_position = placed[previous].expect("placed");
                            (current_position.y - previous_position.y).atan2(current_position.x - previous_position.x)
                        }
                        None => 0.0,
                    }
                };

                let candidates = [
                    base + turn[current] * PI / 6.0,
                    base - turn[current] * PI / 6.0,
                    base,
                    base + turn[current] * PI / 3.0,
                    base - turn[current] * PI / 3.0,
                    base + 2.0 * PI / 3.0,
                    base - 2.0 * PI / 3.0,
                ];
                let mut best: Option<(f64, Point)> = None;
                // 先按 zigzag → 直行 → 回头的顺序取第一个无碰撞、不交叉的
                // 候选。纯「最大间隙」排序会在平手时绕回起点，把链盘成假环。
                for angle in candidates {
                    let point =
                        Point::new(current_position.x + length * angle.cos(), current_position.y + length * angle.sin());
                    let gap = clearance(point, &placed, &[]);
                    let crosses = spec.bonds.iter().any(|bond| {
                        let (Some(from), Some(to)) = (placed[bond.from], placed[bond.to])
                        else {
                            return false;
                        };
                        (bond.from != neighbour && bond.to != neighbour)
                            && segments_cross(current_position, point, from, to)
                    });
                    if gap >= length * 0.9 && !crosses {
                        best = Some((gap, point));
                        break;
                    }
                    if best.as_ref().is_none_or(|(best_gap, _)| gap > *best_gap) {
                        best = Some((gap, point));
                    }
                }
                let Some((gap, point)) = best
                else {
                    return Err(DepictionError::unplaceable(format!("atom {neighbour} has no candidate position")));
                };
                if gap < length * 0.5 {
                    return Err(DepictionError::unplaceable(format!(
                        "atom {neighbour} collides with an already placed atom ({gap:.1} < half a bond length)"
                    )));
                }
                if gap < length {
                    diagnostics.push(named_loss(&format!("atom {neighbour} is placed tighter than one bond length")));
                }
                placed[neighbour] = Some(point);
                parent[neighbour] = Some(current);
                turn[neighbour] = -turn[current];
                queue.push_back(neighbour);
            }
        }
    }

    let positions = placed
        .into_iter()
        .map(|position| position.ok_or_else(|| DepictionError::unplaceable("an atom stayed without a position")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((positions, rings))
}

/// 未放置锚点所在的连通分量是否接触任何已放置原子。
fn touches_placed(anchor: usize, placed: &[Option<Point>], adj: &[Vec<usize>]) -> bool {
    let mut seen = vec![false; placed.len()];
    let mut stack = vec![anchor];
    while let Some(node) = stack.pop() {
        if seen[node] {
            continue;
        }
        seen[node] = true;
        for &neighbour in &adj[node] {
            if placed[neighbour].is_some() {
                return true;
            }
            if !seen[neighbour] {
                stack.push(neighbour);
            }
        }
    }
    false
}

/// 命名损失诊断。
fn named_loss(message: &str) -> Diagnostic {
    diagnostic("depiction", codes::DEPICTION_APPROXIMATED, DiagnosticSeverity::Warning, message.to_string())
}

/// 原子标签文本。`hydrogens` 是显式值或按价推出的隐含氢数；`bonded` 表示该原子
/// 是否与其它原子成键，未成键的氧/硫氢化物按 `H2O` / `H2S` 的惯例把氢写在前面。
fn label_text(atom: &Atom, hydrogens: Option<u8>, bonded: bool) -> String {
    let mut text = String::new();
    if let Some(isotope) = atom.isotope {
        text.push_str(&isotope.to_string());
    }
    let count = hydrogens.unwrap_or(0);
    let hydrogen_leads = !bonded && count > 1 && matches!(atom.element, Element::O | Element::S);
    if hydrogen_leads {
        text.push('H');
        if count > 1 {
            text.push_str(&count.to_string());
        }
    }
    text.push_str(atom.element.symbol());
    if !hydrogen_leads && count > 0 {
        text.push('H');
        if count > 1 {
            text.push_str(&count.to_string());
        }
    }
    match atom.charge {
        0 => {}
        charge if charge > 0 => {
            if charge > 1 {
                text.push_str(&charge.to_string());
            }
            text.push('+');
        }
        charge => {
            if charge < -1 {
                text.push_str(&(-charge).to_string());
            }
            text.push('−');
        }
    }
    text
}

/// 一个分子渲染出的线与标签，以及局部包围盒。
struct Fragment {
    lines: Vec<LinePlan>,
    labels: Vec<LabelPlan>,
    width: f64,
    height: f64,
    min: Point,
}

/// 画一个分子：骨架式。
///
/// 2D 结构图行业通行的画法（ChemDraw / PubChem / Wolfram `MoleculePlot`）：
/// 碳骨架由顶点表达不画符号，键保持黑色细线；彩色只出现在元素文字上
/// （氧红、氮蓝、硫黄、卤素绿紫）。实心圆球的球棍画法属于 3D 模型，
/// 放进 2D 图只会显得廉价。
///
/// 芳香性用「环内侧虚线」表达：每条芳香键只画一根实线，内侧线按环整体
/// 逐边补画并偏向环心。按单条键的固定侧偏移会在环绕向不一致时把虚线
/// 甩到环外、稠环里互相穿插。
fn draw_molecule(spec: &MoleculeSpec, positions: &[Point], length: f64, palette: Palette, rings: &[Vec<usize>]) -> Fragment {
    let label_size = 0.44 * length;
    let mut lines = Vec::new();
    let mut labels = Vec::new();
    let colored = palette == Palette::Element;

    // 显式氢优先；未写氢时按价与已用键级推出隐含氢，避免把 `OH` 画成裸 `O`。
    let hydrogens: Vec<Option<u8>> = spec
        .atoms
        .iter()
        .enumerate()
        .map(|(index, atom)| {
            if atom.hydrogens.is_some() {
                return atom.hydrogens;
            }
            let base = atom.element.base_valence()?;
            let used: f64 =
                spec.bonds.iter().filter(|bond| bond.from == index || bond.to == index).map(|bond| order_value(bond.order)).sum();
            let allowed = f64::from(base as i16 + i16::from(atom.charge)).max(0.0);
            let remaining = allowed - used;
            if remaining < -EPS {
                return None;
            }
            Some(remaining.round().clamp(0.0, 4.0) as u8)
        })
        .collect();

    let texts: Vec<Option<String>> = spec
        .atoms
        .iter()
        .enumerate()
        .map(|(index, atom)| {
            atom.labelled()
                .then(|| label_text(atom, hydrogens[index], spec.bonds.iter().any(|bond| bond.from == index || bond.to == index)))
        })
        .collect();

    for bond in &spec.bonds {
        let (from_atom, to_atom) = (&spec.atoms[bond.from], &spec.atoms[bond.to]);
        let from = positions[bond.from];
        let to = positions[bond.to];
        let dx = to.x - from.x;
        let dy = to.y - from.y;
        let distance = dx.hypot(dy).max(EPS);
        let unit = Point::new(dx / distance, dy / distance);
        let normal = Point::new(-unit.y, unit.x);
        let shrink_from = if from_atom.labelled() { 0.55 * label_size } else { 0.0 };
        let shrink_to = if to_atom.labelled() { 0.55 * label_size } else { 0.0 };
        let start = Point::new(from.x + unit.x * shrink_from, from.y + unit.y * shrink_from);
        let end = Point::new(to.x - unit.x * shrink_to, to.y - unit.y * shrink_to);

        let offset_line = |offset: f64| {
            LinePlan::new(
                Point::new(start.x + normal.x * offset, start.y + normal.y * offset),
                Point::new(end.x + normal.x * offset, end.y + normal.y * offset),
                0.045 * length,
            )
        };

        match bond.order {
            BondOrder::Single => lines.push(offset_line(0.0)),
            BondOrder::Double => {
                lines.push(offset_line(DOUBLE_OFFSET * length));
                lines.push(offset_line(-DOUBLE_OFFSET * length));
            }
            BondOrder::Triple => {
                lines.push(offset_line(TRIPLE_OFFSET * length));
                lines.push(offset_line(0.0));
                lines.push(offset_line(-TRIPLE_OFFSET * length));
            }
            BondOrder::Aromatic => lines.push(offset_line(0.0)),
        }
    }

    // 芳香环内侧虚线：逐环、逐边偏向环心。共享键两侧各补一条，正是稠环的
    // 标准画法。
    let order_at = |a: usize, b: usize| -> Option<BondOrder> {
        spec.bonds
            .iter()
            .find(|bond| (bond.from == a && bond.to == b) || (bond.from == b && bond.to == a))
            .map(|bond| bond.order)
    };
    for ring in rings {
        if ring.len() < 3 || ring.len() > MAX_RING {
            continue;
        }
        let edges: Vec<(usize, usize)> =
            ring.iter().copied().zip(ring.iter().copied().skip(1).chain(ring.first().copied())).collect();
        if !edges.iter().all(|(a, b)| order_at(*a, *b) == Some(BondOrder::Aromatic)) {
            continue;
        }
        let centroid = Point::new(
            ring.iter().map(|atom| positions[*atom].x).sum::<f64>() / ring.len() as f64,
            ring.iter().map(|atom| positions[*atom].y).sum::<f64>() / ring.len() as f64,
        );
        let offset = AROMATIC_OFFSET * length;
        for (a, b) in edges {
            let (from, to) = (positions[a], positions[b]);
            let dx = to.x - from.x;
            let dy = to.y - from.y;
            let distance = dx.hypot(dy).max(EPS);
            let unit = Point::new(dx / distance, dy / distance);
            let mut normal = Point::new(-unit.y, unit.x);
            let mid = Point::new((from.x + to.x) * 0.5, (from.y + to.y) * 0.5);
            if (centroid.x - mid.x) * normal.x + (centroid.y - mid.y) * normal.y < 0.0 {
                normal = Point::new(-normal.x, -normal.y);
            }
            let start = Point::new(from.x + unit.x * 0.15 * length, from.y + unit.y * 0.15 * length);
            let end = Point::new(to.x - unit.x * 0.15 * length, to.y - unit.y * 0.15 * length);
            let inner = LinePlan::new(
                Point::new(start.x + normal.x * offset, start.y + normal.y * offset),
                Point::new(end.x + normal.x * offset, end.y + normal.y * offset),
                0.04 * length,
            )
            .dashed();
            lines.push(inner);
        }
    }

    for (index, atom) in spec.atoms.iter().enumerate() {
        let Some(text) = texts[index].clone()
        else {
            continue;
        };
        labels.push(LabelPlan {
            text,
            // 文本锚点是基线；下移 0.35 em 让符号视觉上落在原子中心。
            at: Point::new(positions[index].x, positions[index].y + 0.35 * label_size),
            size: label_size,
            anchor: TextAnchor::Middle,
            backdrop: true,
            // 彩色调色板按元素给文字上色；单色印刷保持默认墨色。
            color: colored.then(|| element_color(&atom.element)),
        });
    }

    let mut min = Point::new(f64::INFINITY, f64::INFINITY);
    let mut max = Point::new(f64::NEG_INFINITY, f64::NEG_INFINITY);
    let mut extend = |point: Point, half: f64| {
        min.x = min.x.min(point.x - half);
        min.y = min.y.min(point.y - half);
        max.x = max.x.max(point.x + half);
        max.y = max.y.max(point.y + half);
    };
    for line in &lines {
        extend(line.from, 0.0);
        extend(line.to, 0.0);
    }
    for label in &labels {
        // 标签近似为正方形，宽高按字符数估算。
        let half = 0.36 * label.size * (label.text.chars().count().max(1) as f64);
        extend(label.at, half);
    }
    if !min.x.is_finite() {
        min = Point::ZERO;
        max = Point::new(length, length);
    }

    Fragment { lines, labels, width: max.x - min.x, height: max.y - min.y, min }
}

/// 平移一个片段。
fn shift(fragment: &mut Fragment, dx: f64, dy: f64) {
    for line in &mut fragment.lines {
        line.from = Point::new(line.from.x + dx, line.from.y + dy);
        line.to = Point::new(line.to.x + dx, line.to.y + dy);
    }
    for label in &mut fragment.labels {
        label.at = Point::new(label.at.x + dx, label.at.y + dy);
    }
    fragment.min = Point::new(fragment.min.x + dx, fragment.min.y + dy);
}

/// 描绘一个分子。
pub fn depict_molecule(spec: &MoleculeSpec, options: &DepictionOptions) -> Result<DepictionPlan, DepictionError> {
    let length = options.bond_length;
    validate(spec)?;
    let mut diagnostics = Vec::new();
    let (positions, rings) = layout(spec, length, &mut diagnostics)?;
    let mut fragment = draw_molecule(spec, &positions, length, options.palette, &rings);

    // 归一化到 margin 起点的正坐标。
    let dx = options.margin - fragment.min.x;
    let dy = options.margin - fragment.min.y;
    shift(&mut fragment, dx, dy);

    let canvas = Size::new(fragment.width + 2.0 * options.margin, fragment.height + 2.0 * options.margin);
    Ok(DepictionPlan { canvas, lines: fragment.lines, labels: fragment.labels, arrows: Vec::new(), diagnostics })
}

/// 描绘一个反应式：反应物 + 箭头 + 产物，按横向序列排布。
pub fn depict_reaction(reaction: &ReactionSpec, options: &DepictionOptions) -> Result<DepictionPlan, DepictionError> {
    let length = options.bond_length;
    if reaction.reactants.is_empty() || reaction.products.is_empty() {
        return Err(DepictionError::unsupported("a reaction needs at least one reactant and one product"));
    }
    let mut diagnostics = Vec::new();
    let mut fragments: Vec<Fragment> = Vec::new();
    for molecule in reaction.reactants.iter().chain(reaction.products.iter()) {
        validate(molecule)?;
        let (positions, rings) = layout(molecule, length, &mut diagnostics)?;
        fragments.push(draw_molecule(molecule, &positions, length, options.palette, &rings));
    }

    let mut lines: Vec<LinePlan> = Vec::new();
    let mut labels: Vec<LabelPlan> = Vec::new();
    let mut arrows = Vec::new();

    let gap = 0.9 * length;
    let arrow_length = 4.0 * length;
    let reactant_count = reaction.reactants.len();
    let note_color = (options.palette == Palette::Element).then_some(NOTE_GREY);

    // 先排反应物与加号，再放箭头，最后排产物。
    let mut cursor = 0.0;
    let mut tallest: f64 = 0.0;
    for (index, fragment) in fragments.iter_mut().enumerate() {
        let is_reactant = index < reactant_count;
        let position_index = if is_reactant { index } else { index - reactant_count };
        let count = if is_reactant { reactant_count } else { reaction.products.len() };

        if is_reactant && position_index > 0 {
            labels.push(LabelPlan {
                text: "+".to_string(),
                at: Point::new(cursor + gap * 0.5, 0.0),
                size: 0.5 * length,
                anchor: TextAnchor::Middle,
                backdrop: false,
                color: note_color,
            });
            cursor += gap;
        }
        shift(fragment, cursor - fragment.min.x, -fragment.min.y);
        tallest = tallest.max(fragment.height);
        cursor += fragment.width;
        lines.extend(fragment.lines.iter().cloned());
        labels.extend(fragment.labels.iter().cloned());

        if is_reactant && position_index + 1 < count {
            cursor += gap;
        }
        if is_reactant && position_index + 1 == count {
            // 反应物结束：插入箭头。
            cursor += gap;
            let arrow_from = Point::new(cursor, 0.0);
            let arrow_to = Point::new(cursor + arrow_length, 0.0);
            let above = reaction.above.as_ref().map(|text| LabelPlan {
                text: text.clone(),
                at: Point::new((arrow_from.x + arrow_to.x) * 0.5, -0.55 * length),
                size: 0.36 * length,
                anchor: TextAnchor::Middle,
                backdrop: false,
                color: note_color,
            });
            let below = reaction.below.as_ref().map(|text| LabelPlan {
                text: text.clone(),
                at: Point::new((arrow_from.x + arrow_to.x) * 0.5, 0.75 * length),
                size: 0.36 * length,
                anchor: TextAnchor::Middle,
                backdrop: false,
                color: note_color,
            });
            arrows.push(crate::plan::ArrowPlan { from: arrow_from, to: arrow_to, kind: reaction.arrow, above, below });
            cursor += arrow_length + gap;
        }
    }

    // 让画布容纳所有线、标签与箭头（y 方向居中由画布原点处理）。
    let mut min = Point::new(0.0, 0.0);
    let mut max = Point::new(cursor, tallest);
    for line in &lines {
        min.x = min.x.min(line.from.x).min(line.to.x);
        max.x = max.x.max(line.from.x).max(line.to.x);
        min.y = min.y.min(line.from.y).min(line.to.y);
        max.y = max.y.max(line.from.y).max(line.to.y);
    }
    for arrow in &arrows {
        min.y = min.y.min(arrow.from.y).max(-0.7 * length);
        max.y = max.y.max(arrow.from.y).max(1.0 * length);
    }
    for label in &labels {
        min.x = min.x.min(label.at.x);
        max.x = max.x.max(label.at.x + label.size);
        min.y = min.y.min(label.at.y - label.size);
        max.y = max.y.max(label.at.y + label.size);
    }

    let dx = options.margin - min.x;
    let dy = options.margin - min.y;
    for line in &mut lines {
        line.from = Point::new(line.from.x + dx, line.from.y + dy);
        line.to = Point::new(line.to.x + dx, line.to.y + dy);
    }
    for label in &mut labels {
        label.at = Point::new(label.at.x + dx, label.at.y + dy);
    }
    for arrow in &mut arrows {
        arrow.from = Point::new(arrow.from.x + dx, arrow.from.y + dy);
        arrow.to = Point::new(arrow.to.x + dx, arrow.to.y + dy);
        if let Some(above) = &mut arrow.above {
            above.at = Point::new(above.at.x + dx, above.at.y + dy);
        }
        if let Some(below) = &mut arrow.below {
            below.at = Point::new(below.at.x + dx, below.at.y + dy);
        }
    }

    let canvas = Size::new(max.x - min.x + 2.0 * options.margin, max.y - min.y + 2.0 * options.margin);
    Ok(DepictionPlan { canvas, lines, labels, arrows, diagnostics })
}

/// 箭头类型在计划里的绘制宽度（供渲染取线宽）。
pub fn arrow_width(kind: ArrowKind) -> f64 {
    match kind {
        ArrowKind::Single => 0.06,
        ArrowKind::Equilibrium => 0.05,
        ArrowKind::Double => 0.06,
    }
}
