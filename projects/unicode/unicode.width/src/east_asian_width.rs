use crate::tables::east_asian_width::EAST_ASIAN_WIDTH;

/// Unicode `East_Asian_Width` 属性的六个取值。
///
/// 变体对应 UAX #11 的 `A` / `F` / `H` / `N` / `Na` / `W`，[`EastAsianWidth::as_str`]
/// 返回同一拼写。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EastAsianWidth {
    /// `N`：Neutral。
    Neutral,
    /// `A`：Ambiguous。
    Ambiguous,
    /// `H`：Halfwidth。
    Halfwidth,
    /// `F`：Fullwidth。
    Fullwidth,
    /// `W`：Wide。
    Wide,
    /// `Na`：Narrow。
    Narrow,
}

impl EastAsianWidth {
    /// 返回 UAX #11 的属性拼写，例如 `W`、`Na`。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Neutral => "N",
            Self::Ambiguous => "A",
            Self::Halfwidth => "H",
            Self::Fullwidth => "F",
            Self::Wide => "W",
            Self::Narrow => "Na",
        }
    }
}

/// 返回单个标量的 `East_Asian_Width`。
///
/// 覆盖 `U+0000..=U+10FFFF` 全部码点，包括非 BMP；文件未列出的码点按 UCD 的
/// `@missing` 约定取值。
pub fn east_asian_width(ch: char) -> EastAsianWidth {
    let code_point = ch as u32;
    let mut low = 0usize;
    let mut high = EAST_ASIAN_WIDTH.len();
    while low < high {
        let mid = (low + high) / 2;
        let (start, end, width) = EAST_ASIAN_WIDTH[mid];
        if code_point < start {
            high = mid;
        }
        else if code_point > end {
            low = mid + 1;
        }
        else {
            return width;
        }
    }
    // 表覆盖完整码点空间，此处仅为让查询保持全函数；越界码点语义上就是 `N`。
    EastAsianWidth::Neutral
}
