use unicode_types::{GeneralCategory, general_category, is_default_ignorable, is_grapheme_extend};

use crate::east_asian_width::{EastAsianWidth, east_asian_width};

/// `Ambiguous`（`A`）字符在目标终端里的显示策略。
///
/// 终端的实际渲染取决于字体与地区设置，本 crate 不读取宿主环境，只按显式 profile 决定。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum AmbiguousWidth {
    /// 按 1 列计算，适合非 CJK 语境，也是默认策略。
    #[default]
    Narrow,
    /// 按 2 列计算，适合 CJK 语境。
    Wide,
}

/// 计算列宽时使用的策略集合。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct WidthProfile {
    /// `Ambiguous` 字符的显示策略。
    pub ambiguous: AmbiguousWidth,
}

/// Hangul 组合字母（`Conjoining_Jamo` 的中声 `V` 与终声 `T`）。
///
/// Unicode 第 3 章把 `U+1160..=U+11FF` 与扩展区的这两个子区间固定为组合用的中声与
/// 终声字母，它们不单独占据字宽。`HangulSyllableType.txt` 在本仓不可入库，因此这些
/// 区间以常量表达，**不是**从 UCD 生成的数据表。
fn is_hangul_conjoining_jamo(ch: char) -> bool {
    matches!(
        ch,
        '\u{1160}'..='\u{11A7}' | '\u{11A8}'..='\u{11FF}' | '\u{D7B0}'..='\u{D7C6}' | '\u{D7CB}'..='\u{D7FB}'
    )
}

/// 旧实现额外按 0 宽处理的少量码点。
///
/// - 五个 `Prepended_Concatenation_Mark`：`U+0605`、`U+070F`、`U+0890`、`U+0891`、`U+08E2`。
/// - `U+A8FA` DEVANAGARI CARET。
///
/// 这六个码点既不是 `Grapheme_Extend` 也不在 `Default_Ignorable_Code_Point` 内，但旧实现
/// 一律按 0 宽处理，这里保持同一口径。
fn is_extra_zero_width(ch: char) -> bool {
    matches!(ch, '\u{0605}' | '\u{070F}' | '\u{0890}' | '\u{0891}' | '\u{08E2}' | '\u{A8FA}')
}

/// 旧实现按 0 宽处理的 `Grapheme_Cluster_Break=Prepend` 码点。
///
/// 这些码点不属于 `Cf`，也不在 `Default_Ignorable_Code_Point` 内：它们只在字素簇开头
/// 与后续字符合成一簇（如 Malayalam reph、Sharada jihvamuliya）。`GraphemeBreakProperty.txt`
/// 不在本仓入库数据内，故以常量列出 Unicode 18.0.0 的这十个码点。
fn is_prepend_zero_width(ch: char) -> bool {
    matches!(
        ch,
        '\u{0D4E}'
            | '\u{111C2}'..='\u{111C3}'
            | '\u{113D1}'
            | '\u{1193F}'
            | '\u{11941}'
            | '\u{11A84}'..='\u{11A89}'
            | '\u{11D46}'
            | '\u{11F02}'
    )
}

/// 单个字符的显示列宽，控制字符返回 `None`。
///
/// 规则（与 `unicode-width 0.2.3` 对全标量的判定一致）：
///
/// - `Cc` / `Cs`（控制字符与代理项）返回 `None`，调用方自行决定按多少列处理。
/// - `U+2D7F`（Tifinagh 连接符）返回 `1`，`U+115F`、`U+17A4` 返回 `2`，`U+17D8` 返回 `3`。
/// - `Grapheme_Extend`、`Default_Ignorable_Code_Point`、Hangul 组合字母 `V`/`T`、
///   少数 `Prepended_Concatenation_Mark` 与 `Grapheme_Cluster_Break=Prepend` 码点返回 `0`。
/// - `East_Asian_Width` 为 `W` / `F` 时返回 `2`，为 `A` 时按 [`AmbiguousWidth::Narrow`]
///   返回 `1`。
/// - 其余返回 `1`。
///
/// 这里只回答单个码点的列宽。emoji ZWJ 序列、区域指示符对与变体选择符的组合宽度取决于
/// 渲染层，不能由逐字符求和得到。
pub fn char_width(ch: char) -> Option<usize> {
    char_width_with(ch, WidthProfile::default())
}

/// 按给定 profile 返回单个字符的显示列宽，控制字符返回 `None`。
///
/// 与 [`char_width`] 的唯一区别是 `East_Asian_Width` 为 `A` 的字符按
/// [`WidthProfile::ambiguous`] 取 1 或 2。
pub fn char_width_with(ch: char, profile: WidthProfile) -> Option<usize> {
    if matches!(general_category(ch), GeneralCategory::Cc | GeneralCategory::Cs) {
        return None;
    }
    match ch {
        // 旧实现显式按 1 列计的码点：Tifinagh 连接符与半角假名浊音/半浊音符号，
        // 尽管它们都属于 `Grapheme_Extend`。
        '\u{2D7F}' | '\u{FF9E}' | '\u{FF9F}' => return Some(1),
        // Hangul 填充字母与 Khmer 独立元音 QAA 显式按 2 列计。
        '\u{115F}' | '\u{17A4}' => return Some(2),
        // Khmer 缩写符 BEYYAL 显式按 3 列计。
        '\u{17D8}' => return Some(3),
        _ => {}
    }
    if is_grapheme_extend(ch)
        || is_default_ignorable(ch)
        || is_hangul_conjoining_jamo(ch)
        || is_extra_zero_width(ch)
        || is_prepend_zero_width(ch)
    {
        return Some(0);
    }
    Some(match east_asian_width(ch) {
        EastAsianWidth::Wide | EastAsianWidth::Fullwidth => 2,
        EastAsianWidth::Ambiguous => match profile.ambiguous {
            AmbiguousWidth::Narrow => 1,
            AmbiguousWidth::Wide => 2,
        },
        EastAsianWidth::Neutral | EastAsianWidth::Halfwidth | EastAsianWidth::Narrow => 1,
    })
}

/// 逐字符求和得到文本的显示列宽，控制字符按 1 列计入。
///
/// 结果就是 [`char_width`] 的逐字符求和，其中返回 `None` 的控制字符按 `1` 计入，与
/// `char_width(ch).unwrap_or(1)` 的约定一致。
///
/// 本函数不处理 emoji ZWJ 序列、区域指示符对与变体选择符的组合：这些序列在真实终端里
/// 往往整体占 2 列而不是各字符之和。需要精确的序列宽度时由渲染层按序列规则计算，本函数
/// 只提供与上下文无关的逐字符估计。
pub fn str_width(text: &str) -> usize {
    str_width_with(text, WidthProfile::default())
}

/// 按给定 profile 逐字符求和得到文本的显示列宽，控制字符按 1 列计入。
///
/// 序列限制与 [`str_width`] 相同。
pub fn str_width_with(text: &str, profile: WidthProfile) -> usize {
    text.chars().map(|ch| char_width_with(ch, profile).unwrap_or(1)).sum()
}
