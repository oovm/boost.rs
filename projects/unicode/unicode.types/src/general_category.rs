use crate::tables::general_category::GENERAL_CATEGORY;

/// Unicode `General_Category` 的全部 30 个取值。
///
/// 变体名即 UCD 的两字母缩写，[`GeneralCategory::as_str`] 返回同一拼写。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeneralCategory {
    /// `Lu`：Uppercase_Letter。
    Lu,
    /// `Ll`：Lowercase_Letter。
    Ll,
    /// `Lt`：Titlecase_Letter。
    Lt,
    /// `Lm`：Modifier_Letter。
    Lm,
    /// `Lo`：Other_Letter。
    Lo,
    /// `Mn`：Nonspacing_Mark。
    Mn,
    /// `Mc`：Spacing_Mark。
    Mc,
    /// `Me`：Enclosing_Mark。
    Me,
    /// `Nd`：Decimal_Number。
    Nd,
    /// `Nl`：Letter_Number。
    Nl,
    /// `No`：Other_Number。
    No,
    /// `Pc`：Connector_Punctuation。
    Pc,
    /// `Pd`：Dash_Punctuation。
    Pd,
    /// `Ps`：Open_Punctuation。
    Ps,
    /// `Pe`：Close_Punctuation。
    Pe,
    /// `Pi`：Initial_Punctuation。
    Pi,
    /// `Pf`：Final_Punctuation。
    Pf,
    /// `Po`：Other_Punctuation。
    Po,
    /// `Sm`：Math_Symbol。
    Sm,
    /// `Sc`：Currency_Symbol。
    Sc,
    /// `Sk`：Modifier_Symbol。
    Sk,
    /// `So`：Other_Symbol。
    So,
    /// `Zs`：Space_Separator。
    Zs,
    /// `Zl`：Line_Separator。
    Zl,
    /// `Zp`：Paragraph_Separator。
    Zp,
    /// `Cc`：Control。
    Cc,
    /// `Cf`：Format。
    Cf,
    /// `Cs`：Surrogate。
    Cs,
    /// `Co`：Private_Use。
    Co,
    /// `Cn`：Unassigned。
    Cn,
}

impl GeneralCategory {
    /// 返回 UCD 的两字母缩写，例如 `Lu`、`Mn`、`Cn`。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lu => "Lu",
            Self::Ll => "Ll",
            Self::Lt => "Lt",
            Self::Lm => "Lm",
            Self::Lo => "Lo",
            Self::Mn => "Mn",
            Self::Mc => "Mc",
            Self::Me => "Me",
            Self::Nd => "Nd",
            Self::Nl => "Nl",
            Self::No => "No",
            Self::Pc => "Pc",
            Self::Pd => "Pd",
            Self::Ps => "Ps",
            Self::Pe => "Pe",
            Self::Pi => "Pi",
            Self::Pf => "Pf",
            Self::Po => "Po",
            Self::Sm => "Sm",
            Self::Sc => "Sc",
            Self::Sk => "Sk",
            Self::So => "So",
            Self::Zs => "Zs",
            Self::Zl => "Zl",
            Self::Zp => "Zp",
            Self::Cc => "Cc",
            Self::Cf => "Cf",
            Self::Cs => "Cs",
            Self::Co => "Co",
            Self::Cn => "Cn",
        }
    }
}

/// 返回单个标量的 `General_Category`。
///
/// 覆盖 `U+0000..=U+10FFFF` 全部码点，未分配与非字符码点归 `Cn`。
pub fn general_category(ch: char) -> GeneralCategory {
    let code_point = ch as u32;
    let mut low = 0usize;
    let mut high = GENERAL_CATEGORY.len();
    while low < high {
        let mid = (low + high) / 2;
        let (start, end, category) = GENERAL_CATEGORY[mid];
        if code_point < start {
            high = mid;
        }
        else if code_point > end {
            low = mid + 1;
        }
        else {
            return category;
        }
    }
    // 表覆盖完整码点空间，此处仅为让查询保持全函数；越界码点语义上就是未分配。
    GeneralCategory::Cn
}

/// 判断标量是否属于 Mark 类（`Mn` / `Mc` / `Me`）。
pub fn is_mark(ch: char) -> bool {
    matches!(general_category(ch), GeneralCategory::Mn | GeneralCategory::Mc | GeneralCategory::Me)
}
