//! SMuFL 码位常量。
//!
//! 谱面字形按 SMuFL 1.x 码位发出，`font-family` 固定为 `Bravura`（OFL-1.1），
//! 字体字节由下游（VuTeX 字体包）注入，本仓零二进制入库。em 与谱表间距的换算
//! 约定：SMuFL 设计 em = 4 个谱表间距。

/// G 谱号（高音谱号）。
pub const G_CLEF: char = '\u{E050}';
/// F 谱号（低音谱号）。
pub const F_CLEF: char = '\u{E052}';
/// 拍号数字 0 的起始码位，0–9 连续。
pub const TIME_DIGIT_0: u32 = 0xE080;
/// 普通四四拍号（C）。
pub const TIME_COMMON: char = '\u{E08A}';
/// 切分四四拍号。
pub const TIME_CUT: char = '\u{E08B}';
/// 全音符头。
pub const NOTE_WHOLE: char = '\u{E0A2}';
/// 二分音符头。
pub const NOTE_HALF: char = '\u{E0A3}';
/// 四分音符头。
pub const NOTE_QUARTER: char = '\u{E0A4}';
/// 八分音符头。
pub const NOTE_EIGHTH: char = '\u{E0A5}';
/// 十六分音符头。
pub const NOTE_16TH: char = '\u{E0A6}';
/// 降号。
pub const ACCIDENTAL_FLAT: char = '\u{E260}';
/// 还原号。
pub const ACCIDENTAL_NATURAL: char = '\u{E261}';
/// 升号。
pub const ACCIDENTAL_SHARP: char = '\u{E262}';
/// 重升号。
pub const ACCIDENTAL_DOUBLE_SHARP: char = '\u{E263}';
/// 重降号。
pub const ACCIDENTAL_DOUBLE_FLAT: char = '\u{E264}';
/// 上向八分符尾。
pub const FLAG_8TH_UP: char = '\u{E240}';
/// 下向八分符尾。
pub const FLAG_8TH_DOWN: char = '\u{E241}';
/// 上向十六分符尾。
pub const FLAG_16TH_UP: char = '\u{E242}';
/// 下向十六分符尾。
pub const FLAG_16TH_DOWN: char = '\u{E243}';
/// 全休止符。
pub const REST_WHOLE: char = '\u{E4E2}';
/// 二分休止符。
pub const REST_HALF: char = '\u{E4E3}';
/// 四分休止符。
pub const REST_QUARTER: char = '\u{E4E4}';
/// 八分休止符。
pub const REST_8TH: char = '\u{E4E5}';
/// 十六分休止符。
pub const REST_16TH: char = '\u{E4E6}';

/// 拍号数字字符（0–9）的码位。
pub fn time_digit(digit: u8) -> char {
    char::from_u32(TIME_DIGIT_0 + u32::from(digit.min(9))).unwrap_or(TIME_COMMON)
}
