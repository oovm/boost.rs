//! `ScoreSpec` → `EngravingPlan` 的确定性排版编译。
//!
//! 布局规则（v0）：
//!
//! - 场景单位 = 谱表间距 `space`；SMuFL 字形 em = 4 `space`。
//! - 时值按比例分配水平空间，附点折入 `duration`，连音压缩因子走 `ratio`。
//! - 行系统按 `max_line_width` 装包，每行重复谱号、调号与拍号。
//! - 符杠按符尾自动分组（八分及以下连续音符），显式 `Beam` 覆盖自动判定；
//!   v0 符杠为平杠，组内符杆端点跨度超过半个间距时记一条 `approximated`。
//! - 小节不满拍号按隐式休止补齐并记 `approximated`；超出拍号整例拒绝。
//! - 多声部跨谱表、曲中改调改拍、三十秒音符及更短均 fail-closed。

use plotter_scene::{PathData, TextAnchor, TextStyle};
use plotter_types::{Color, Diagnostic, DiagnosticSeverity, Point, Size, codes, diagnostic};

use crate::{
    error::EngravingError,
    plan::{
        ArcPlan, BarlinePlan, BeamPlan, DotPlan, EngravingOptions, EngravingPlan, GlyphPlan, LinePlan, NotePlan, SystemPlan,
        TextPlan,
    },
    smufl,
    spec::{
        Accidental, Articulation, Barline, Beam, Clef, EventKind, KeySignature, Pitch, ScoreSpec, SlurState, Step, TieState,
        TimeSignature,
    },
};

/// 浮点时值比较容差。
const EPS: f64 = 1e-9;
/// 视觉时值基（四分音符单位），从长到短。
const BASES: [f64; 5] = [4.0, 2.0, 1.0, 0.5, 0.25];
/// 符杆长度（谱表间距倍数）。
const STEM_LENGTH: f64 = 3.5;
/// 符杠厚度。
const BEAM_THICK: f64 = 0.5;
/// 符杠与最近符杆端点的间隙。
const BEAM_GAP: f64 = 0.25;
/// 十六分双杠偏移。
const BEAM_OFFSET: f64 = 1.0;
/// 音符最小水平间距。
const MIN_EVENT_GAP: f64 = 1.8;
/// 页边距（谱表间距倍数）。
const MARGIN: f64 = 2.0;

/// 把视觉时值分解为符头基与附点数。
fn split_duration(duration: f64) -> Option<(f64, u8)> {
    for base in BASES {
        for dots in 0..=2u8 {
            let value = base * (2.0 - 0.5f64.powi(i32::from(dots) - 1));
            if (value - duration).abs() < EPS {
                return Some((base, dots));
            }
        }
    }
    None
}

/// 符头基对应的 SMuFL 码位。
fn head_glyph(base: f64) -> char {
    match base {
        4.0 => smufl::NOTE_WHOLE,
        2.0 => smufl::NOTE_HALF,
        1.0 => smufl::NOTE_QUARTER,
        0.5 => smufl::NOTE_EIGHTH,
        _ => smufl::NOTE_16TH,
    }
}

/// 休止符对应的 SMuFL 码位。
fn rest_glyph(base: f64) -> char {
    match base {
        4.0 => smufl::REST_WHOLE,
        2.0 => smufl::REST_HALF,
        1.0 => smufl::REST_QUARTER,
        0.5 => smufl::REST_8TH,
        _ => smufl::REST_16TH,
    }
}

/// 符头基对应的宽度（谱表间距倍数）。
fn head_advance(base: f64) -> f64 {
    match base {
        4.0 => 1.76,
        2.0 => 1.32,
        _ => 1.14,
    }
}

/// 临时记号宽度（谱表间距倍数）。Bravura 单升降号墨迹约 1.0 个间距，
/// 重升重降约 1.8。
fn accidental_advance(accidental: Accidental) -> f64 {
    match accidental {
        Accidental::Sharp | Accidental::Flat | Accidental::Natural => 1.0,
        Accidental::DoubleSharp | Accidental::DoubleFlat => 1.8,
    }
}

/// 临时记号 SMuFL 码位。
fn accidental_glyph(accidental: Accidental) -> char {
    match accidental {
        Accidental::Flat => smufl::ACCIDENTAL_FLAT,
        Accidental::Natural => smufl::ACCIDENTAL_NATURAL,
        Accidental::Sharp => smufl::ACCIDENTAL_SHARP,
        Accidental::DoubleSharp => smufl::ACCIDENTAL_DOUBLE_SHARP,
        Accidental::DoubleFlat => smufl::ACCIDENTAL_DOUBLE_FLAT,
    }
}

/// 调号按五度序的音级序列。
const SHARP_ORDER: [Step; 7] = [Step::F, Step::C, Step::G, Step::D, Step::A, Step::E, Step::B];
const FLAT_ORDER: [Step; 7] = [Step::B, Step::E, Step::A, Step::D, Step::G, Step::C, Step::F];

/// 调号影响的音级集合。
fn key_steps(key: KeySignature) -> Vec<(Step, Accidental)> {
    match key {
        KeySignature::Natural => Vec::new(),
        KeySignature::Sharps(count) => {
            SHARP_ORDER[..usize::from(count.min(7))].iter().map(|step| (*step, Accidental::Sharp)).collect()
        }
        KeySignature::Flats(count) => {
            FLAT_ORDER[..usize::from(count.min(7))].iter().map(|step| (*step, Accidental::Flat)).collect()
        }
    }
}

/// 调号字形位置（音级 → 谱表自然音位置表按谱号取）。
fn key_positions(clef: Clef, key: KeySignature) -> Vec<(char, i16)> {
    let (count, glyph, order): (usize, char, &[Step]) = match key {
        KeySignature::Natural => return Vec::new(),
        KeySignature::Sharps(count) => (usize::from(count.min(7)), smufl::ACCIDENTAL_SHARP, &SHARP_ORDER),
        KeySignature::Flats(count) => (usize::from(count.min(7)), smufl::ACCIDENTAL_FLAT, &FLAT_ORDER),
    };
    // 高音谱表：F5 C5 G5 D5 A4 E5 B4；低音谱表：F3 C3 G3 D3 A2 E3 B2。
    let treble: [i16; 7] = [38, 35, 39, 36, 33, 37, 34];
    let bass: [i16; 7] = [24, 21, 27, 22, 19, 25, 20];
    let table: &[i16] = match clef {
        Clef::Treble => &treble,
        Clef::Bass => &bass,
    };
    order[..count].iter().enumerate().map(|(index, _)| (glyph, table[index])).collect()
}

/// 音级在调号下的效果。
fn key_effect(key: KeySignature, step: Step) -> Option<Accidental> {
    key_steps(key).into_iter().find(|(s, _)| *s == step).map(|(_, a)| a)
}

/// 自然音位置 → 谱表 y（场景单位，绝对值需加系统 origin.y）。
fn staff_y(position: i16, clef: Clef, space: f64) -> f64 {
    let (anchor, anchor_y): (i16, f64) = match clef {
        Clef::Treble => (30, 4.0),
        Clef::Bass => (26, 0.0),
    };
    (anchor_y - f64::from(position - anchor) * 0.5) * space
}

/// 拍号的每小节四分单位时值。
fn measure_capacity(time: TimeSignature) -> Result<f64, EngravingError> {
    match time {
        TimeSignature::Common | TimeSignature::Cut => Ok(4.0),
        TimeSignature::Explicit { beats, beat_unit } => {
            if beats == 0 || !matches!(beat_unit, 1 | 2 | 4 | 8 | 16) {
                return Err(EngravingError::unsupported(format!(
                    "time signature `{beats}/{beat_unit}` is outside the v0 subset (beats 1-16, unit 1, 2, 4, 8, or 16)"
                )));
            }
            Ok(f64::from(beats) * 4.0 / f64::from(beat_unit))
        }
    }
}

/// 校验 spec 并返回每小节时值。
fn validate(spec: &ScoreSpec) -> Result<Vec<f64>, EngravingError> {
    if spec.measures.is_empty() || spec.measures.iter().all(|m| m.events.is_empty()) {
        return Err(EngravingError::empty_score("the score has no measures with events to engrave"));
    }
    if let KeySignature::Sharps(count) | KeySignature::Flats(count) = spec.key
        && count > 7
    {
        return Err(EngravingError::unsupported(format!(
            "key signature with {count} accidentals exceeds the 7 sharp or flat limit"
        )));
    }
    let capacity = measure_capacity(spec.time)?;
    let mut durations = Vec::with_capacity(spec.measures.len());
    for (index, measure) in spec.measures.iter().enumerate() {
        let mut total = 0.0;
        for event in &measure.events {
            if event.duration <= 0.0 {
                return Err(EngravingError::unsupported(format!(
                    "measure {} declares a non-positive duration {}",
                    index + 1,
                    event.duration
                )));
            }
            if split_duration(event.duration).is_none() {
                return Err(EngravingError::unsupported(format!(
                    "duration {} in measure {} is outside the v0 subset (whole through 16th, up to two dots)",
                    event.duration,
                    index + 1
                )));
            }
            if let EventKind::Chord(pitches) = &event.kind
                && pitches.is_empty()
            {
                return Err(EngravingError::unsupported(format!("measure {} declares an empty chord", index + 1)));
            }
            if let Some(tuplet) = event.tuplet
                && (tuplet.actual == 0 || tuplet.normal == 0)
            {
                return Err(EngravingError::unsupported(format!("measure {} declares a zero-sized tuplet", index + 1)));
            }
            total += event.duration * event.ratio.max(EPS);
        }
        if total > capacity + EPS {
            return Err(EngravingError::unsupported(format!(
                "measure {} holds {total} quarter units, over the capacity {capacity} of the time signature",
                index + 1
            )));
        }
        durations.push(total);
    }
    Ok(durations)
}

/// 小节自然宽度（谱表间距倍数）。
fn measure_width_spaces(dur: f64, event_count: usize) -> f64 {
    let natural = 4.0 + 6.0 * dur;
    let min_by_events = 2.0 + MIN_EVENT_GAP * event_count as f64;
    natural.max(min_by_events)
}

/// 头部（谱号 + 调号 + 拍号）宽度（谱表间距倍数）。
///
/// 高音谱号的字形从基线向上约 2.3 个间距、向下约 1.5 个间距，右侧墨迹到
/// 约 1.6 个间距处；拍号必须整体排在它右边，否则 4/4 会压在谱号肚子上。
fn header_width_spaces(key: KeySignature) -> f64 {
    let key_count = match key {
        KeySignature::Natural => 0,
        KeySignature::Sharps(c) | KeySignature::Flats(c) => usize::from(c.min(7)),
    };
    2.6 + f64::from(key_count as u32) * 1.1 + f64::from(key_count > 0) * 0.4 + 1.2
}

/// 事件几何（用于跨小节 / 跨行配对 tie 与 slur）。
struct EventGeom {
    system: usize,
    x: f64,
    y: f64,
    half_width: f64,
    stem_up: bool,
    tie: TieState,
    slur: SlurState,
}

/// 一个小节内已放置的事件（局部，符杠分组前）。
struct PlacedEvent {
    note: NotePlan,
    x: f64,
    y: f64,
    half_width: f64,
    stem_up: bool,
    stem_x: f64,
    flagged: bool,
    sixteenth: bool,
    beam: Beam,
    tie: TieState,
    slur: SlurState,
    tuplet_actual: u8,
}

/// 排版一张谱。
pub fn engrave(spec: &ScoreSpec, options: &EngravingOptions) -> Result<EngravingPlan, EngravingError> {
    let space = options.staff_space;
    let durations = validate(spec)?;
    let capacity = measure_capacity(spec.time)?;

    let header_spaces = header_width_spaces(spec.key);
    let line_budget = options.max_line_width - 2.0 * MARGIN * space;
    if line_budget <= header_spaces * space + 6.0 * space {
        return Err(EngravingError::unsupported("max_line_width leaves no room for a staff header plus one measure"));
    }

    // 1. 行系统装包（跳过空小节）。
    let mut systems: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut used = 0.0;
    for (index, measure) in spec.measures.iter().enumerate() {
        if measure.events.is_empty() {
            continue;
        }
        let width = measure_width_spaces(durations[index], measure.events.len()) * space;
        if !current.is_empty() && used + width > line_budget {
            systems.push(std::mem::take(&mut current));
            used = 0.0;
        }
        if current.is_empty() {
            used += header_spaces * space;
        }
        current.push(index);
        used += width;
    }
    if !current.is_empty() {
        systems.push(current);
    }

    // 2. 逐行放置。
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut system_plans: Vec<SystemPlan> = Vec::new();
    let mut geoms: Vec<EventGeom> = Vec::new();
    let mut y_cursor = MARGIN * space + if spec.title.is_some() { 4.5 * space } else { 0.0 };
    let mut max_width = 0.0f64;

    for (system_index, system_indices) in systems.iter().enumerate() {
        let origin = Point::new(MARGIN * space, y_cursor);
        let mut plan = SystemPlan {
            origin,
            width: 0.0,
            space,
            header_glyphs: Vec::new(),
            notes: Vec::new(),
            beams: Vec::new(),
            arcs: Vec::new(),
            barlines: Vec::new(),
            repeat_dots: Vec::new(),
            tuplet_numbers: Vec::new(),
            tempo: None,
            tempo_glyph: None,
        };

        // 头部：谱号、调号、拍号。
        let mut x = origin.x;
        let clef_glyph = match spec.clef {
            Clef::Treble => smufl::G_CLEF,
            Clef::Bass => smufl::F_CLEF,
        };
        // SMuFL 谱号基线锚在自己的定位线上：gClef 在 G4 线（下数第二条），
        // fClef 在 F3 线（上数第二条）。
        let clef_baseline = match spec.clef {
            Clef::Treble => 3.5 * space,
            Clef::Bass => 1.0 * space,
        };
        plan.header_glyphs.push(GlyphPlan::bravura(clef_glyph, Point::new(x, origin.y + clef_baseline), space).with_id("clef"));
        x += 2.6 * space;
        for (key_index, (glyph, position)) in key_positions(spec.clef, spec.key).iter().enumerate() {
            let y = origin.y + staff_y(*position, spec.clef, space);
            plan.header_glyphs.push(GlyphPlan::bravura(*glyph, Point::new(x, y), space).with_id(format!("key-{key_index}")));
            x += 1.1 * space;
        }
        push_time_glyphs(&mut plan, spec.time, origin, space, x);
        x = origin.x + header_spaces * space;

        // 首行放速度。速度标记整体高于谱号上缘，避免与 40pt 的 G 谱号相撞。
        if system_index == 0
            && let Some(bpm) = spec.tempo_bpm
        {
            let baseline = origin.y - 2.5 * space;
            plan.tempo_glyph = Some(
                GlyphPlan::bravura(smufl::NOTE_QUARTER, Point::new(origin.x, baseline), space).with_id("tempo-glyph"),
            );
            plan.tempo = Some(body_text(
                &format!("= {bpm}"),
                Point::new(origin.x + 1.2 * space, baseline),
                1.3 * space,
                TextAnchor::Start,
            ));
        }

        // 小节与事件。
        for &measure_index in system_indices {
            let measure = &spec.measures[measure_index];
            let dur = durations[measure_index];
            let width = measure_width_spaces(dur, measure.events.len()) * space;
            let usable = width - 3.0 * space;
            let mut placed: Vec<PlacedEvent> = Vec::with_capacity(measure.events.len());
            let mut cursor = x + 1.5 * space;
            let mut cum = 0.0;

            for (event_index, event) in measure.events.iter().enumerate() {
                let (base, dots) = split_duration(event.duration).expect("validated");
                let proportional = if dur > EPS { x + 1.5 * space + usable * (cum / dur) } else { x + 1.5 * space };
                let note_x = proportional.max(cursor);

                let mut note = NotePlan {
                    heads: Vec::new(),
                    accidentals: Vec::new(),
                    stem: None,
                    flags: Vec::new(),
                    ledger_lines: Vec::new(),
                    articulation_dots: Vec::new(),
                    articulation_lines: Vec::new(),
                    lyric: None,
                    x: note_x,
                };
                let mut half_width: f64 = 0.0;

                match &event.kind {
                    EventKind::Rest => {
                        let rest_y = match base {
                            4.0 => origin.y + 1.0 * space,
                            2.0 => origin.y + 3.0 * space,
                            _ => origin.y + 2.0 * space,
                        };
                        note.heads.push(
                            GlyphPlan::bravura(rest_glyph(base), Point::new(note_x, rest_y), space)
                                .with_id(format!("event-{measure_index}-{event_index}")),
                        );
                        cursor = note_x + 1.2 * space + MIN_EVENT_GAP * space;
                    }
                    EventKind::Note(_) | EventKind::Chord(_) => {
                        let pitches: Vec<Pitch> = match &event.kind {
                            EventKind::Note(pitch) => vec![*pitch],
                            EventKind::Chord(pitches) => pitches.clone(),
                            EventKind::Rest => Vec::new(),
                        };
                        let mut ys = Vec::with_capacity(pitches.len());
                        for (pitch_index, pitch) in pitches.iter().enumerate() {
                            let y = origin.y + staff_y(pitch.position(), spec.clef, space);
                            ys.push(y);
                            note.heads.push(
                                GlyphPlan::bravura(head_glyph(base), Point::new(note_x, y), space)
                                    .with_id(format!("event-{measure_index}-{event_index}-{pitch_index}")),
                            );
                            if let Some(accidental) = event_accidental(spec, pitch) {
                                let adv = accidental_advance(accidental) * space;
                                note.accidentals.push(
                                    GlyphPlan::bravura(
                                        accidental_glyph(accidental),
                                        Point::new(note_x - adv - 0.15 * space, y),
                                        space,
                                    )
                                    .with_id(format!("acc-{measure_index}-{event_index}-{pitch_index}")),
                                );
                                half_width = half_width.max(adv + 0.15 * space);
                            }
                            if dots > 0 {
                                let dot_x = note_x + head_advance(base) * space + 0.15 * space;
                                let dot_y = dot_offset(y, space);
                                note.articulation_dots.push(DotPlan { center: Point::new(dot_x, dot_y), radius: 0.12 * space });
                                if dots == 2 {
                                    note.articulation_dots
                                        .push(DotPlan { center: Point::new(dot_x + 0.5 * space, dot_y), radius: 0.12 * space });
                                }
                            }
                            push_ledgers(&mut note, y - origin.y, space);
                        }
                        let avg = ys.iter().sum::<f64>() / ys.len() as f64;
                        let stem_up = avg >= origin.y + 2.0 * space;
                        half_width = half_width.max(head_advance(base) * space * 0.5);
                        let stem_x =
                            if stem_up { note_x + head_advance(base) * space - 0.08 * space } else { note_x - 0.08 * space };
                        let tip_y = avg + if stem_up { -STEM_LENGTH * space } else { STEM_LENGTH * space };
                        // 全音符无符杆；二分及更短都有符杆。
                        if base < 4.0 - EPS {
                            note.stem = Some(
                                LinePlan::new(Point::new(stem_x, avg), Point::new(stem_x, tip_y), 0.12 * space)
                                    .with_id(format!("stem-{measure_index}-{event_index}")),
                            );
                        }
                        // 有符尾的音符先画符尾；成组符杠时再统一剥掉。
                        let flagged = base <= 0.5 + EPS;
                        if flagged {
                            push_flags(&mut note, stem_x, tip_y, stem_up, base <= 0.25 + EPS, space);
                        }
                        for articulation in &event.articulations {
                            match articulation {
                                Articulation::Staccato => {
                                    let y = if stem_up { avg + 0.7 * space } else { avg - 0.7 * space };
                                    note.articulation_dots
                                        .push(DotPlan { center: Point::new(note_x + 0.5 * space, y), radius: 0.16 * space });
                                }
                                Articulation::Tenuto => {
                                    let y = if stem_up { avg + 0.6 * space } else { avg - 0.6 * space };
                                    note.articulation_lines.push(LinePlan::new(
                                        Point::new(note_x, y),
                                        Point::new(note_x + head_advance(base) * space, y),
                                        0.12 * space,
                                    ));
                                }
                            }
                        }
                        if let Some(lyric) = &event.lyric {
                            note.lyric = Some(body_text(
                                lyric,
                                Point::new(note_x + head_advance(base) * space * 0.5, origin.y + 6.5 * space),
                                1.3 * space,
                                TextAnchor::Middle,
                            ));
                        }
                        cursor = note_x + half_width + MIN_EVENT_GAP * space;
                        placed.push(PlacedEvent {
                            note,
                            x: note_x,
                            y: avg,
                            half_width,
                            stem_up,
                            stem_x,
                            flagged,
                            sixteenth: base <= 0.25 + EPS,
                            beam: event.beam,
                            tie: event.tie,
                            slur: event.slur,
                            tuplet_actual: event.tuplet.map_or(0, |t| t.actual),
                        });
                    }
                }
                cum += event.duration * event.ratio.max(EPS);
            }

            // 符杠分组与连音括号（小节内）。
            beam_groups(&mut plan, &mut placed, space, &mut diagnostics);
            tuplet_brackets(&mut plan, &placed, space);

            for event in placed {
                let PlacedEvent { note, x, y, half_width, stem_up, tie, slur, .. } = event;
                geoms.push(EventGeom { system: system_index, x, y, half_width, stem_up, tie, slur });
                plan.notes.push(note);
            }

            // 小节线。
            let bar_x = (cursor - MIN_EVENT_GAP * space).max(x + width);
            plan.barlines.push(BarlinePlan { x: bar_x, kind: measure.barline });
            match measure.barline {
                Barline::RepeatEnd => {
                    plan.repeat_dots.push(DotPlan {
                        center: Point::new(bar_x - 0.7 * space, origin.y + 1.5 * space),
                        radius: 0.15 * space,
                    });
                    plan.repeat_dots.push(DotPlan {
                        center: Point::new(bar_x - 0.7 * space, origin.y + 2.5 * space),
                        radius: 0.15 * space,
                    });
                }
                Barline::RepeatStart => {
                    plan.repeat_dots.push(DotPlan {
                        center: Point::new(bar_x + 0.9 * space, origin.y + 1.5 * space),
                        radius: 0.15 * space,
                    });
                    plan.repeat_dots.push(DotPlan {
                        center: Point::new(bar_x + 0.9 * space, origin.y + 2.5 * space),
                        radius: 0.15 * space,
                    });
                }
                Barline::Regular | Barline::Final => {}
            }

            if dur < capacity - EPS {
                diagnostics.push(named_loss(&format!(
                    "measure {} holds {dur} quarter units below the capacity {capacity}; an implicit rest pads its tail",
                    measure_index + 1
                )));
            }
            x = bar_x + 1.5 * space;
        }
        plan.width = x - origin.x;
        max_width = max_width.max(plan.width);
        y_cursor += 8.5 * space;
        system_plans.push(plan);
    }

    // 3. 跨小节 / 跨行 tie 与 slur 配对。
    pair_arcs(&mut system_plans, &geoms);

    // 4. 画布。
    let canvas = Size::new(
        max_width + 2.0 * MARGIN * space,
        y_cursor - 2.0 * space + if spec.composer.is_some() { 2.5 * space } else { 0.0 },
    );
    let title = spec.title.as_ref().map(|text| {
        body_text(text, Point::new(canvas.width * 0.5, MARGIN * space + 0.8 * space), 2.0 * space, TextAnchor::Middle)
    });
    let composer = spec.composer.as_ref().map(|text| {
        body_text(text, Point::new(canvas.width - MARGIN * space, canvas.height - 1.2 * space), 1.4 * space, TextAnchor::End)
    });

    Ok(EngravingPlan { canvas, space, systems: system_plans, title, composer, diagnostics })
}

/// 事件是否显示临时记号：显式给出且与调号效果不同才画。
fn event_accidental(spec: &ScoreSpec, pitch: &Pitch) -> Option<Accidental> {
    let authored = pitch.accidental?;
    if key_effect(spec.key, pitch.step) == Some(authored) {
        return None;
    }
    Some(authored)
}

/// 附点 y：线音上移半间到相邻间。
fn dot_offset(y: f64, space: f64) -> f64 {
    let units = y / space;
    if (units - units.round()).abs() < EPS { y - 0.5 * space } else { y }
}

/// 加线（参数 y 为系统内相对值）。
fn push_ledgers(note: &mut NotePlan, y: f64, space: f64) {
    let units = y / space;
    if units <= -1.0 + EPS {
        let mut line = -1.0;
        while line >= units - EPS {
            note.ledger_lines.push(LinePlan::new(
                Point::new(note.x - 0.9 * space, line * space),
                Point::new(note.x + 0.9 * space, line * space),
                0.12 * space,
            ));
            line -= 1.0;
        }
    }
    if units >= 5.0 - EPS {
        let mut line = 5.0;
        while line <= units + EPS {
            note.ledger_lines.push(LinePlan::new(
                Point::new(note.x - 0.9 * space, line * space),
                Point::new(note.x + 0.9 * space, line * space),
                0.12 * space,
            ));
            line += 1.0;
        }
    }
}

/// 拍号字形。`x0` 是谱号与调号之后的光标位置，拍号必须排在它们右侧。
fn push_time_glyphs(plan: &mut SystemPlan, time: TimeSignature, origin: Point, space: f64, x0: f64) {
    match time {
        TimeSignature::Common | TimeSignature::Cut => {
            let glyph = if time == TimeSignature::Common { smufl::TIME_COMMON } else { smufl::TIME_CUT };
            plan.header_glyphs
                .push(GlyphPlan::bravura(glyph, Point::new(x0 + 0.2 * space, origin.y + 2.0 * space), space).with_id("time"));
        }
        TimeSignature::Explicit { beats, beat_unit } => {
            let mut x = x0 + 0.2 * space;
            for digit in digits(beats) {
                plan.header_glyphs.push(
                    GlyphPlan::bravura(smufl::time_digit(digit), Point::new(x, origin.y + 2.0 * space), space)
                        .with_id("time-num"),
                );
                x += 0.5 * space;
            }
            let mut x = x0 + 0.2 * space;
            for digit in digits(beat_unit) {
                plan.header_glyphs.push(
                    GlyphPlan::bravura(smufl::time_digit(digit), Point::new(x, origin.y + 4.0 * space), space)
                        .with_id("time-den"),
                );
                x += 0.5 * space;
            }
        }
    }
}

/// 十进制位（高位在前）。
fn digits(value: u8) -> Vec<u8> {
    if value == 0 {
        return vec![0];
    }
    let mut out = Vec::new();
    let mut v = value;
    while v > 0 {
        out.push((v % 10) as u8);
        v /= 10;
    }
    out.reverse();
    out
}

/// 正文字体文本。
fn body_text(text: &str, at: Point, size: f64, anchor: TextAnchor) -> TextPlan {
    TextPlan {
        text: text.to_string(),
        at,
        style: TextStyle {
            fill: Some(Color::BLACK),
            opacity: 1.0,
            font_family: None,
            font_size: size,
            font_weight: None,
            text_anchor: anchor,
        },
    }
}

/// 命名损失诊断。
fn named_loss(message: &str) -> Diagnostic {
    diagnostic("engraving", codes::ENGRAVING_APPROXIMATED, DiagnosticSeverity::Warning, message.to_string())
}

/// 符尾字形：八分一枚，十六分两枚。
fn push_flags(note: &mut NotePlan, stem_x: f64, tip_y: f64, stem_up: bool, sixteenth: bool, space: f64) {
    let (first, second) =
        if stem_up { (smufl::FLAG_8TH_UP, smufl::FLAG_16TH_UP) } else { (smufl::FLAG_8TH_DOWN, smufl::FLAG_16TH_DOWN) };
    let x = if stem_up { stem_x + 0.02 * space } else { stem_x - 1.1 * space };
    note.flags.push(GlyphPlan::bravura(first, Point::new(x, tip_y), space));
    if sixteenth {
        let y = tip_y + if stem_up { -0.75 * space } else { 0.75 * space };
        note.flags.push(GlyphPlan::bravura(second, Point::new(x, y), space));
    }
}

/// 小节内符杠分组：连续 flagged 音符成组，组内 ≥2 时发符杠并剥符尾。
fn beam_groups(plan: &mut SystemPlan, placed: &mut [PlacedEvent], space: f64, diagnostics: &mut Vec<Diagnostic>) {
    let mut start: Option<usize> = None;
    for index in 0..=placed.len() {
        let eligible = placed.get(index).is_some_and(|event| event.flagged && event.beam != Beam::NoBeam);
        let continues = match placed.get(index).map(|event| event.beam) {
            Some(Beam::Begin) => false,
            _ => true,
        };
        let closes = match placed.get(index).map(|event| event.beam) {
            Some(Beam::End) => true,
            _ => false,
        };
        if index > 0
            && let Some(group_start) = start
            && (!eligible || !continues || placed[index - 1].beam == Beam::End)
        {
            if index - group_start >= 2 {
                emit_beam(plan, &placed[group_start..index], space, diagnostics);
                for event in &mut placed[group_start..index] {
                    event.note.flags.clear();
                }
            }
            start = None;
        }
        if eligible && (placed.get(index).is_some_and(|event| event.beam == Beam::Begin) || start.is_none()) {
            start = Some(index);
        }
        let _ = closes;
    }
}

/// 发一条平杠；端点跨度超半间记命名损失。
fn emit_beam(plan: &mut SystemPlan, group: &[PlacedEvent], space: f64, diagnostics: &mut Vec<Diagnostic>) {
    let first = group.first().expect("group checked");
    let last = group.last().expect("group checked");
    let tips: Vec<f64> =
        group.iter().map(|event| event.y + if event.stem_up { -STEM_LENGTH * space } else { STEM_LENGTH * space }).collect();
    let lo = tips.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = tips.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    if hi - lo > 0.5 * space {
        diagnostics.push(named_loss("a beam group whose stem tips span more than half a staff space is drawn as a flat beam"));
    }
    let beam_y = if first.stem_up { lo - BEAM_GAP * space - BEAM_THICK * space } else { hi + BEAM_GAP * space };
    let x0 = first.stem_x;
    let x1 = last.stem_x + 0.12 * space;
    let mut bands = vec![[
        Point::new(x0, beam_y),
        Point::new(x1, beam_y),
        Point::new(x1, beam_y + BEAM_THICK * space),
        Point::new(x0, beam_y + BEAM_THICK * space),
    ]];
    if group.iter().any(|event| event.sixteenth) {
        let offset = if first.stem_up { -BEAM_OFFSET * space } else { BEAM_OFFSET * space };
        bands.push([
            Point::new(x0, beam_y + offset),
            Point::new(x1, beam_y + offset),
            Point::new(x1, beam_y + offset + BEAM_THICK * space),
            Point::new(x0, beam_y + offset + BEAM_THICK * space),
        ]);
    }
    plan.beams.push(BeamPlan { bands });
}

/// 连音组括号与数字：连续带连音标记的事件按 `actual` 聚成一组。
fn tuplet_brackets(plan: &mut SystemPlan, placed: &[PlacedEvent], space: f64) {
    let mut group: Vec<usize> = Vec::new();
    let mut actual = 0u8;
    for (index, event) in placed.iter().enumerate() {
        if event.tuplet_actual > 0 {
            if group.is_empty() {
                actual = event.tuplet_actual;
            }
            group.push(index);
            if group.len() as u8 >= actual.max(1) {
                emit_tuplet(plan, &placed[group[0]..=index], space, actual);
                group.clear();
            }
        }
        else if !group.is_empty() {
            let last = *group.last().expect("non-empty");
            emit_tuplet(plan, &placed[group[0]..=last], space, actual);
            group.clear();
        }
    }
    if let Some(first) = group.first().copied() {
        let last = *group.last().expect("non-empty");
        emit_tuplet(plan, &placed[first..=last], space, actual);
    }
}

fn emit_tuplet(plan: &mut SystemPlan, group: &[PlacedEvent], space: f64, actual: u8) {
    let Some(first) = group.first()
    else {
        return;
    };
    let last = group.last().expect("non-empty");
    let up = first.stem_up;
    let y = if up { first.y - (STEM_LENGTH + 0.8) * space } else { first.y + (STEM_LENGTH + 0.8) * space };
    let x0 = first.x;
    let x1 = last.x + last.half_width;
    let tick = if up { 0.4 * space } else { -0.4 * space };
    let path = PathData::new()
        .move_to(Point::new(x0, y + tick))
        .line_to(Point::new(x0, y))
        .line_to(Point::new(x1, y))
        .line_to(Point::new(x1, y + tick));
    plan.arcs.push(ArcPlan { path, width: 0.1 * space });
    plan.tuplet_numbers.push(body_text(
        &actual.to_string(),
        Point::new((x0 + x1) * 0.5, if up { y - 0.4 * space } else { y + 1.4 * space }),
        1.2 * space,
        TextAnchor::Middle,
    ));
}

/// 跨小节 / 跨行配对 tie 与 slur 并发出弧线。
fn pair_arcs(systems: &mut [SystemPlan], geoms: &[EventGeom]) {
    let mut tie_open: Option<usize> = None;
    let mut slur_open: Option<usize> = None;
    for (index, geom) in geoms.iter().enumerate() {
        if geom.tie == TieState::Start {
            tie_open = Some(index);
        }
        else if geom.tie == TieState::Stop
            && let Some(start) = tie_open.take()
        {
            push_arc(systems, &geoms[start], geom, 0.7);
        }
        if geom.slur == SlurState::Start {
            slur_open = Some(index);
        }
        else if geom.slur == SlurState::Stop
            && let Some(start) = slur_open.take()
        {
            push_arc(systems, &geoms[start], geom, 1.2);
        }
    }
}

/// 发一条弧（在符杆对侧）。
fn push_arc(systems: &mut [SystemPlan], from: &EventGeom, to: &EventGeom, bulge: f64) {
    let Some(space) = systems.get(from.system).map(|system| system.space)
    else {
        return;
    };
    let above = from.stem_up;
    let offset = if above { -0.6 * space } else { 0.6 * space };
    let y0 = from.y + offset;
    let y1 = to.y + offset;
    let mid_x = (from.x + to.x) * 0.5;
    let mid_y = (y0 + y1) * 0.5 + if above { -bulge * space } else { bulge * space };
    let path = PathData::new()
        .move_to(Point::new(from.x + from.half_width, y0))
        .quadratic_to(Point::new(mid_x, mid_y), Point::new(to.x - to.half_width, y1));
    if let Some(system) = systems.get_mut(from.system) {
        system.arcs.push(ArcPlan { path, width: 0.12 * space });
    }
}
