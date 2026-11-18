use unicode_types::{GeneralCategory, general_category};

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

/// 返回单个字符的显示列宽，控制字符返回 `None`。
///
/// 规则：
///
/// - `Cc` / `Cs`（控制字符与代理项）返回 `None`，调用方自行决定按多少列处理。
/// - Mark 类（`Mn` / `Mc` / `Me`）与 `Cf` 返回 `0`。
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
    match general_category(ch) {
        GeneralCategory::Cc | GeneralCategory::Cs => None,
        GeneralCategory::Mn | GeneralCategory::Mc | GeneralCategory::Me | GeneralCategory::Cf => Some(0),
        _ => Some(match east_asian_width(ch) {
            EastAsianWidth::Wide | EastAsianWidth::Fullwidth => 2,
            EastAsianWidth::Ambiguous => match profile.ambiguous {
                AmbiguousWidth::Narrow => 1,
                AmbiguousWidth::Wide => 2,
            },
            EastAsianWidth::Neutral | EastAsianWidth::Halfwidth | EastAsianWidth::Narrow => 1,
        }),
    }
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
