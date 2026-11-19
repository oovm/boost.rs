#![warn(missing_docs)]
#![doc = include_str!("readme.md")]

//! ABC 记谱前端：`data-text-abc` 的 AST 降级进 `plotter.engraving` 的与源语言无关模型。
//!
//! 分工与 DOT 前端一致：文本语法由 `data-text-abc` 解析并保留字节跨度，本 crate 只做
//! 「AST → [`ScoreSpec`]」的语义映射，再由 `plotter.engraving` 排版成场景。
//! 无法表达或会改变含义的构造分两类处理：
//!
//! - 整谱无法正确排版（中音谱号、教会调式调号、非 1/2/4/8/16 单位拍）返回
//!   [`EngravingError`]，宿主保留作者源码并记录 `davinci.engraving.unsupported`。
//! - 局部含义丢失（被忽略的第二个 tune/voice、`accent` 等无字形装饰、双小节线
//!   降成普通小节线）保留可排版的部分，并记录一条
//!   `davinci.engraving.approximated` 命名损失。

use std::ops::Range;

use data_text_abc::{
    AbcBuilder, AbcLanguage,
    ast::{
        AbcAccidental, AbcBarline, AbcChord, AbcClef, AbcDecoration, AbcEvent, AbcFraction, AbcKey, AbcMeter, AbcNote, AbcPitch, AbcRoot,
        AbcSlur, AbcUnsupported,
    },
};
use data_text_core::{Builder, SourceText, errors::TextError, parser::session::ParseSession};
use plotter_engraving::{
    EngravingError, EngravingOptions, LoweredEngraving,
    spec::{
        Accidental, Articulation, Barline, Clef, Event, KeySignature, Measure, Pitch, ScoreSpec, SlurState, Step, TieState, TimeSignature,
        Tuplet,
    },
};
use plotter_types::{
    Diagnostic, DiagnosticLabel, DiagnosticLocation, DiagnosticSeverity, LabelRole, Message, NAMESPACE, SourceRef, codes, diagnostic,
};

/// 诊断与标签里使用的源身份。
const SOURCE: &str = "abc";

/// ABC 前端降级结果：与源语言无关的谱模型加前端记录的命名损失。
#[derive(Clone, Debug, PartialEq)]
pub struct LoweredScore {
    /// 可交给 `plotter.engraving` 排版的谱模型。
    pub spec: ScoreSpec,
    /// 前端在映射中记录的非致命损失。
    pub diagnostics: Vec<Diagnostic>,
}

/// 解析 ABC 源码，返回保留字节跨度的语法树。
pub fn parse_abc_source(source: &str) -> Result<AbcRoot, EngravingError> {
    let text = SourceText::new(source);
    let builder = AbcBuilder;
    let mut session = ParseSession::<AbcLanguage>::default();
    let output = builder.build(&text, &[], &mut session);
    match output.result {
        Ok(root) => Ok(root),
        Err(error) => Err(spanned_parse_error(source, &error)),
    }
}

/// 把 ABC 源码降级成 `ScoreSpec` 与命名损失，不排版。
pub fn abc_to_spec(source: &str) -> Result<LoweredScore, EngravingError> {
    let root = parse_abc_source(source)?;
    lower_root(&root)
}

/// 把 ABC 源码直接排版成显示场景。
pub fn abc_to_scene(source: &str) -> Result<LoweredEngraving, EngravingError> {
    let lowered = abc_to_spec(source)?;
    let mut engraved = plotter_engraving::score_to_scene_with_options(&lowered.spec, &EngravingOptions::default())?;
    engraved.plan.diagnostics.extend(lowered.diagnostics);
    engraved.diagnostics = engraved.plan.diagnostics.clone();
    Ok(engraved)
}

/// 把解析错误转成带跨度的拒绝。
///
/// 语法错误携带出问题的字节偏移，拒绝因此指向具体记号，而不是一条没有位置的
/// 「解析失败」。
fn spanned_parse_error(source: &str, error: &TextError) -> EngravingError {
    if let data_text_core::errors::TextErrorKind::SyntaxError { message, offset, .. } = error.kind() {
        let offset = (*offset).min(source.len());
        let end = source[offset..].find('\n').map(|delta| offset + delta).unwrap_or(source.len());
        return EngravingError::unsupported(message.clone()).with_span(offset..end);
    }
    EngravingError::unsupported(format!("`data-text-abc` failed to build an AST: {error}"))
}

/// 把源字节跨度转成诊断跨度。
fn span_of(range: &core::range::Range<usize>) -> Range<usize> {
    range.start..range.end
}

/// 记录一条带字节跨度的命名损失。
fn approximated(losses: &mut Vec<Diagnostic>, message: impl Into<String>, span: Option<Range<usize>>) {
    let message = message.into();
    let mut record = diagnostic("engraving", codes::ENGRAVING_APPROXIMATED, DiagnosticSeverity::Warning, message.clone());
    let Some(span) = span.filter(|span| span.start < span.end)
    else {
        losses.push(record);
        return;
    };
    let Ok(source) = SourceRef::new(NAMESPACE, SOURCE)
    else {
        losses.push(record);
        return;
    };
    let Ok(range) = plotter_types::ByteRange::new(span.start as u64, span.end as u64)
    else {
        losses.push(record);
        return;
    };
    record = record.with_primary(DiagnosticLabel::new(
        DiagnosticLocation::Text { source, range },
        Message::new(codes::ENGRAVING_APPROXIMATED).with_fallback(message),
        LabelRole::Primary,
    ));
    losses.push(record);
}

/// 记录一条无法表达为 `AbcUnsupported` 的命名损失。
fn refused_construct(losses: &mut Vec<Diagnostic>, unsupported: &AbcUnsupported) {
    approximated(
        losses,
        format!("`{}` is outside the ABC subset and stays as authored text: {}", unsupported.construct, unsupported.source.trim()),
        Some(span_of(&unsupported.span)),
    );
}

/// 降级一张 ABC 文档。
fn lower_root(root: &AbcRoot) -> Result<LoweredScore, EngravingError> {
    let mut diagnostics = Vec::new();
    for unsupported in &root.header.unsupported {
        refused_construct(&mut diagnostics, unsupported);
    }
    if root.tunes.len() > 1 {
        approximated(
            &mut diagnostics,
            format!("the source declares {} tunes; only the first one is engraved", root.tunes.len()),
            Some(span_of(&root.tunes[1].span)),
        );
    }
    let tune = root.tunes.first().ok_or_else(|| EngravingError::unsupported("the ABC source declares no tune to engrave"))?;
    for unsupported in &tune.unsupported {
        refused_construct(&mut diagnostics, unsupported);
    }
    if tune.voices.len() > 1 {
        approximated(
            &mut diagnostics,
            format!("the tune declares {} voices; only the first one is engraved", tune.voices.len()),
            Some(span_of(&tune.voices[1].span)),
        );
    }
    let voice = tune.voices.first().ok_or_else(|| EngravingError::unsupported("the ABC tune declares no voice to engrave"))?;
    let clef = lower_clef(voice.clef.as_ref(), &voice.span)?;
    let header_span = span_of(&root.header.span);
    let key = lower_key(root.header.key.as_ref(), &header_span)?;
    let time = lower_meter(root.header.meter.as_ref(), &header_span)?;

    let mut measures = Vec::with_capacity(voice.measures.len());
    let mut pending_tie = false;
    for measure in &voice.measures {
        let mut events = Vec::with_capacity(measure.events.len());
        for event in &measure.events {
            match event {
                AbcEvent::Note(note) => {
                    events.push(lower_note(note, &mut pending_tie, &mut diagnostics));
                }
                AbcEvent::Rest(rest) => {
                    if pending_tie {
                        approximated(&mut diagnostics, "a tie that runs into a rest is dropped", Some(span_of(&rest.span)));
                        pending_tie = false;
                    }
                    events.push(Event::rest(quarter_units(rest.length)));
                }
                AbcEvent::Chord(chord) => {
                    if let Some(event) = lower_chord(chord, &mut pending_tie, &mut diagnostics) {
                        events.push(event);
                    }
                }
                AbcEvent::Unsupported(unsupported) => refused_construct(&mut diagnostics, unsupported),
            }
        }
        measures.push(Measure { events, barline: lower_barline(measure.barline, &mut diagnostics, &measure.span) });
    }
    if pending_tie {
        approximated(&mut diagnostics, "the final tie starts on the last note and has no target", None);
    }

    let spec = ScoreSpec {
        title: root.header.title.clone(),
        composer: root.header.composer.clone(),
        tempo_bpm: root.header.tempo_bpm,
        clef,
        key,
        time,
        measures,
    };
    Ok(LoweredScore { spec, diagnostics })
}

/// 谱号映射。v0 只有高音与低音谱表。
fn lower_clef(clef: Option<&AbcClef>, span: &core::range::Range<usize>) -> Result<Clef, EngravingError> {
    match clef {
        None | Some(AbcClef::Treble) => Ok(Clef::Treble),
        Some(AbcClef::Bass) => Ok(Clef::Bass),
        Some(AbcClef::Alto) => {
            Err(EngravingError::unsupported("an alto clef is outside the v0 engraving subset, which models treble and bass staves")
                .with_span(span_of(span)))
        }
        Some(AbcClef::Other(name)) => {
            Err(EngravingError::unsupported(format!("clef `{name}` is outside the v0 engraving subset, which models treble and bass staves"))
                .with_span(span_of(span)))
        }
    }
}

/// 拍号映射。
fn lower_meter(meter: Option<&AbcMeter>, header_span: &Range<usize>) -> Result<TimeSignature, EngravingError> {
    match meter {
        None | Some(AbcMeter::Common) => Ok(TimeSignature::Common),
        Some(AbcMeter::Cut) => Ok(TimeSignature::Cut),
        Some(AbcMeter::Explicit { beats, unit }) => {
            if *beats == 0 || !matches!(unit, 1 | 2 | 4 | 8 | 16) {
                return Err(EngravingError::unsupported(format!(
                    "meter `{beats}/{unit}` is outside the v0 subset (beats 1-16, unit 1, 2, 4, 8, or 16)"
                ))
                .with_span(header_span.clone()));
            }
            Ok(TimeSignature::Explicit { beats: *beats, beat_unit: *unit })
        }
    }
}

/// 调号映射：由主音与调式推出五度圈上的升降号个数。
fn lower_key(key: Option<&AbcKey>, header_span: &Range<usize>) -> Result<KeySignature, EngravingError> {
    let Some(key) = key
    else {
        return Ok(KeySignature::Natural);
    };
    let mode = key.mode.trim().to_ascii_lowercase();
    let minor = match mode.as_str() {
        "" | "maj" | "major" => false,
        "m" | "min" | "minor" | "aeolian" => true,
        other => {
            return Err(EngravingError::unsupported(format!("key mode `{other}` is outside the v0 subset, which models major and minor keys"))
                .with_span(header_span.clone()));
        }
    };
    let letter = match key.tonic.to_ascii_uppercase() {
        'C' => 0i32,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        other => {
            return Err(
                EngravingError::unsupported(format!("key tonic `{other}` is not one of C, D, E, F, G, A, B")).with_span(header_span.clone())
            );
        }
    };
    let shift = match key.accidental {
        None => 0,
        Some(AbcAccidental::Sharp) => 1,
        Some(AbcAccidental::DoubleSharp) => 2,
        Some(AbcAccidental::Flat) => -1,
        Some(AbcAccidental::DoubleFlat) => -2,
        Some(AbcAccidental::Natural) => 0,
    };
    let mut pitch_class = (letter + shift).rem_euclid(12);
    if minor {
        // 关系大调：小调主音上移小三度。
        pitch_class = (pitch_class + 3).rem_euclid(12);
    }
    let sharps = (pitch_class * 7).rem_euclid(12);
    if sharps <= 7 {
        Ok(if sharps == 0 { KeySignature::Natural } else { KeySignature::Sharps(sharps as u8) })
    }
    else {
        Ok(KeySignature::Flats((12 - sharps) as u8))
    }
}

/// 小节线映射。v0 不建模双细线，降成普通小节线并记录损失。
fn lower_barline(barline: AbcBarline, diagnostics: &mut Vec<Diagnostic>, span: &core::range::Range<usize>) -> Barline {
    match barline {
        AbcBarline::Regular => Barline::Regular,
        AbcBarline::Final => Barline::Final,
        AbcBarline::RepeatStart => Barline::RepeatStart,
        AbcBarline::RepeatEnd => Barline::RepeatEnd,
        AbcBarline::Double => {
            approximated(diagnostics, "a double bar line is drawn as a regular bar line", Some(span_of(span)));
            Barline::Regular
        }
        AbcBarline::RepeatBoth => {
            approximated(
                diagnostics,
                "a repeat on both sides is drawn as a repeat end, so the following repeat start is lost",
                Some(span_of(span)),
            );
            Barline::RepeatEnd
        }
    }
}

/// 时值：全音符分数 → 以四分音符为 1.0 的场景时值。
fn quarter_units(length: AbcFraction) -> f64 {
    4.0 * f64::from(length.numerator) / f64::from(length.denominator.max(1))
}

/// 音高映射。
fn lower_pitch(pitch: AbcPitch) -> Pitch {
    let step = match pitch.step.to_ascii_uppercase() {
        'D' => Step::D,
        'E' => Step::E,
        'F' => Step::F,
        'G' => Step::G,
        'A' => Step::A,
        'B' => Step::B,
        _ => Step::C,
    };
    let accidental = match pitch.accidental {
        None => None,
        Some(AbcAccidental::Sharp) => Some(Accidental::Sharp),
        Some(AbcAccidental::DoubleSharp) => Some(Accidental::DoubleSharp),
        Some(AbcAccidental::Flat) => Some(Accidental::Flat),
        Some(AbcAccidental::DoubleFlat) => Some(Accidental::DoubleFlat),
        Some(AbcAccidental::Natural) => Some(Accidental::Natural),
    };
    Pitch { step, octave: pitch.octave, accidental }
}

/// 歌词清洗：ABC 的 `w:` 行里 `|` 是小节线占位符，`_` 是延续线；两者都不是
/// 要唱的音节，落在词尾就剥掉。
fn clean_lyric(lyric: Option<&str>) -> Option<String> {
    let text = lyric?.trim_end_matches(['|', '_']).trim_end();
    (!text.is_empty()).then(|| text.to_string())
}

/// 一条音符事件。
fn lower_note(note: &AbcNote, pending_tie: &mut bool, diagnostics: &mut Vec<Diagnostic>) -> Event {
    let mut event = Event::note(lower_pitch(note.pitch), quarter_units(note.length));
    event.tie = tie_state(*pending_tie, note.tie, diagnostics, &note.span);
    *pending_tie = note.tie;
    event.slur = slur_state(note.slur, diagnostics, &note.span);
    event.articulations = articulations(&note.decorations, diagnostics, &note.span);
    event.lyric = clean_lyric(note.lyric.as_deref());
    if let Some(tuplet) = note.tuplet
        && tuplet.actual > 0
        && tuplet.normal > 0
    {
        event.tuplet = Some(Tuplet { actual: tuplet.actual, normal: tuplet.normal });
        event.ratio = f64::from(tuplet.normal) / f64::from(tuplet.actual);
        // `data-text-abc` 已把连音组音符的时值预除成 3:2（三连音八分记作 1/12 全音符）。
        // `ScoreSpec` 的合同是 `duration` 保持书写记号值、压缩只走 `ratio`，
        // 这里把被除掉的因子乘回来，否则排版层会看到 1/3 四分这种符头基外时值。
        event.duration *= f64::from(tuplet.actual) / f64::from(tuplet.normal);
    }
    event
}

/// 一个和弦事件。
///
/// 和弦成员共享同一时值，但各自的连线、装饰与歌词在 `ScoreSpec` 里没有逐音位置。
/// 成员写法不一致时保留第一个音符的附加状态并记录损失。
fn lower_chord(chord: &AbcChord, pending_tie: &mut bool, diagnostics: &mut Vec<Diagnostic>) -> Option<Event> {
    if chord.notes.is_empty() {
        approximated(diagnostics, "an empty chord is dropped", Some(span_of(&chord.span)));
        return None;
    }
    let lyric = chord.notes.iter().find_map(|note| clean_lyric(note.lyric.as_deref()));
    let mut event = Event::chord(chord.notes.iter().map(|note| lower_pitch(note.pitch)).collect(), quarter_units(chord.length));
    let tie = chord.notes.iter().any(|note| note.tie);
    event.tie = tie_state(*pending_tie, tie, diagnostics, &chord.span);
    *pending_tie = tie;
    if chord.notes.iter().any(|note| note.slur != AbcSlur::None) {
        approximated(diagnostics, "a slur on a chord member is not drawn", Some(span_of(&chord.span)));
    }
    let mut decorations: Vec<AbcDecoration> = Vec::new();
    for note in &chord.notes {
        for decoration in &note.decorations {
            if !decorations.contains(decoration) {
                decorations.push(decoration.clone());
            }
        }
    }
    event.articulations = articulations(&decorations, diagnostics, &chord.span);
    event.lyric = lyric;
    Some(event)
}

/// 连线状态：同音连线在 `ScoreSpec` 里只有一个状态位，无法同时收尾与起头。
fn tie_state(pending: bool, starts: bool, diagnostics: &mut Vec<Diagnostic>, span: &core::range::Range<usize>) -> TieState {
    match (pending, starts) {
        (false, false) => TieState::None,
        (false, true) => TieState::Start,
        (true, false) => TieState::Stop,
        (true, true) => {
            approximated(diagnostics, "a note closes one tie and starts another; the incoming tie is dropped", Some(span_of(span)));
            TieState::Start
        }
    }
}

/// 圆滑线状态。
fn slur_state(slur: AbcSlur, diagnostics: &mut Vec<Diagnostic>, span: &core::range::Range<usize>) -> SlurState {
    match slur {
        AbcSlur::None => SlurState::None,
        AbcSlur::Start => SlurState::Start,
        AbcSlur::Stop => SlurState::Stop,
        AbcSlur::Both => {
            approximated(diagnostics, "a slur that both opens and closes on one note is drawn as a closing slur", Some(span_of(span)));
            SlurState::Stop
        }
    }
}

/// 装饰映射。v0 只有断奏与保持线有基本图元。
fn articulations(decorations: &[AbcDecoration], diagnostics: &mut Vec<Diagnostic>, span: &core::range::Range<usize>) -> Vec<Articulation> {
    let mut out = Vec::new();
    for decoration in decorations {
        match decoration {
            AbcDecoration::Staccato => {
                if !out.contains(&Articulation::Staccato) {
                    out.push(Articulation::Staccato);
                }
            }
            AbcDecoration::Tenuto => {
                if !out.contains(&Articulation::Tenuto) {
                    out.push(Articulation::Tenuto);
                }
            }
            AbcDecoration::Accent => {
                approximated(diagnostics, "an accent has no v0 glyph and is not drawn", Some(span_of(span)));
            }
            AbcDecoration::Unknown(name) => {
                approximated(diagnostics, format!("decoration `{name}` is outside the ABC subset and is not drawn"), Some(span_of(span)));
            }
        }
    }
    out
}
