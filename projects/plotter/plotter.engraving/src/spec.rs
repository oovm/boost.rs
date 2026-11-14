//! 与源语言无关的乐谱语义。
//!
//! [`ScoreSpec`] 描述一张谱「是什么」：谱号、调号、拍号、小节与音高事件。它不携带
//! 任何位置、尺寸、绘制命令或源文本细节。语言前端（如 `plotter.input.abc`）降级进
//! 这个模型，[`crate::engrave`] 把它编译成排版计划。

/// 谱号。v0 只建模高音与低音谱号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clef {
    /// G 谱号，下起第一线为 E4。
    Treble,
    /// F 谱号，上起第一线为 A3。
    Bass,
}

/// 音名级数（自然音）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// C
    C,
    /// D
    D,
    /// E
    E,
    /// F
    F,
    /// G
    G,
    /// A
    A,
    /// B
    B,
}

impl Step {
    /// 自然音级索引，C = 0 … B = 6。
    pub fn index(self) -> u8 {
        match self {
            Self::C => 0,
            Self::D => 1,
            Self::E => 2,
            Self::F => 3,
            Self::G => 4,
            Self::A => 5,
            Self::B => 6,
        }
    }
}

/// 升降记号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accidental {
    /// 重降。
    DoubleFlat,
    /// 降。
    Flat,
    /// 还原。
    Natural,
    /// 升。
    Sharp,
    /// 重升。
    DoubleSharp,
}

/// 绝对音高：音级 + 科学音高记号八度 + 可选临时记号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pitch {
    /// 自然音级。
    pub step: Step,
    /// 八度号，C4 为中央 C。
    pub octave: i8,
    /// 临时记号；缺省表示由调号决定。
    pub accidental: Option<Accidental>,
}

impl Pitch {
    /// 全音域自然音位置（八度 × 7 + 音级），供谱表定位。
    pub fn position(self) -> i16 {
        i16::from(self.octave) * 7 + i16::from(self.step.index())
    }
}

/// 拍号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeSignature {
    /// 普通四四（C）。
    Common,
    /// 切分四四（C 加竖线）。
    Cut,
    /// 显式分子/分母。
    Explicit {
        /// 每小节拍数，1–16。
        beats: u8,
        /// 单位拍分母，只接受 1、2、4、8、16。
        beat_unit: u8,
    },
}

/// 调号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySignature {
    /// 无升降。
    Natural,
    /// 升号个数，1–7。
    Sharps(u8),
    /// 降号个数，1–7。
    Flats(u8),
}

/// 小节线类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Barline {
    /// 普通细线。
    Regular,
    /// 终止双线（细 + 粗）。
    Final,
    /// 反复结束（细 + 粗 + 左侧两点）。
    RepeatEnd,
    /// 反复开始（粗 + 细 + 右侧两点）。
    RepeatStart,
}

/// 连音线（同音高 tie）状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TieState {
    /// 无。
    None,
    /// 起。
    Start,
    /// 收。
    Stop,
}

/// 圆滑线（slur）状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlurState {
    /// 无。
    None,
    /// 起。
    Start,
    /// 收。
    Stop,
}

/// 符杠归属。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Beam {
    /// 由排版引擎按符尾自动分组。
    Auto,
    /// 显式组起。
    Begin,
    /// 延续当前组。
    Continue,
    /// 显式组收。
    End,
    /// 不参与符杠。
    NoBeam,
}

/// 发音记号。v0 只收可用基本图元绘制、不依赖额外字形的两种。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Articulation {
    /// 断奏（音符上方/下方小圆点）。
    Staccato,
    /// 保持线。
    Tenuto,
}

/// 连音组压缩（如三连音 3:2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tuplet {
    /// 实际音符数。
    pub actual: u8,
    /// 正常时值参照数。
    pub normal: u8,
}

/// 一个时值事件。
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// 事件内容。
    pub kind: EventKind,
    /// 视觉时值，以四分音符为 1.0（附点已折入，如二分附点为 1.5）。
    pub duration: f64,
    /// 时间压缩因子，默认 1.0；连音组内为 `normal / actual`。
    pub ratio: f64,
    /// 符杠归属。
    pub beam: Beam,
    /// 连音线状态。
    pub tie: TieState,
    /// 圆滑线状态。
    pub slur: SlurState,
    /// 发音记号。
    pub articulations: Vec<Articulation>,
    /// 歌词音节。
    pub lyric: Option<String>,
    /// 该事件起始的连音组。
    pub tuplet: Option<Tuplet>,
}

impl Event {
    /// 创建一个无附加状态的音符事件。
    pub fn note(pitch: Pitch, duration: f64) -> Self {
        Self {
            kind: EventKind::Note(pitch),
            duration,
            ratio: 1.0,
            beam: Beam::Auto,
            tie: TieState::None,
            slur: SlurState::None,
            articulations: Vec::new(),
            lyric: None,
            tuplet: None,
        }
    }

    /// 创建一个休止事件。
    pub fn rest(duration: f64) -> Self {
        let mut event = Self::note(Pitch { step: Step::C, octave: 4, accidental: None }, duration);
        event.kind = EventKind::Rest;
        event
    }

    /// 创建一个和弦事件。
    pub fn chord(pitches: Vec<Pitch>, duration: f64) -> Self {
        let mut event = Self::note(Pitch { step: Step::C, octave: 4, accidental: None }, duration);
        event.kind = EventKind::Chord(pitches);
        event
    }
}

/// 事件内容。
#[derive(Clone, Debug, PartialEq)]
pub enum EventKind {
    /// 单音。
    Note(Pitch),
    /// 和弦（同 x 位置叠置）。
    Chord(Vec<Pitch>),
    /// 休止。
    Rest,
}

/// 一个小节。
#[derive(Clone, Debug, PartialEq)]
pub struct Measure {
    /// 小节内事件，按时间顺序。
    pub events: Vec<Event>,
    /// 小节结束处的小节线。
    pub barline: Barline,
}

/// 一张完整乐谱。
#[derive(Clone, Debug, PartialEq)]
pub struct ScoreSpec {
    /// 标题。
    pub title: Option<String>,
    /// 曲名/作者。
    pub composer: Option<String>,
    /// 速度（BPM）。
    pub tempo_bpm: Option<u32>,
    /// 谱号。
    pub clef: Clef,
    /// 调号。
    pub key: KeySignature,
    /// 拍号。
    pub time: TimeSignature,
    /// 小节序列。
    pub measures: Vec<Measure>,
}

impl ScoreSpec {
    /// 创建一张默认高音谱表、C 大调、4/4 的空谱。
    pub fn new() -> Self {
        Self {
            title: None,
            composer: None,
            tempo_bpm: None,
            clef: Clef::Treble,
            key: KeySignature::Natural,
            time: TimeSignature::Common,
            measures: Vec::new(),
        }
    }
}

impl Default for ScoreSpec {
    fn default() -> Self {
        Self::new()
    }
}
