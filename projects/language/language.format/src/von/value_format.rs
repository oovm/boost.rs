//! VON 值文本序列化（委托 `data-text-von::printer` / serde）。

/// 将值序列化为紧凑 VON 文本。
#[cfg(feature = "serde")]
pub fn to_string<T>(value: &T) -> Result<String, data_text_core::TextError>
where
    T: serde::Serialize,
{
    data_text_von::to_string(value)
}

/// 将值序列化为带缩进的 VON 文本。
#[cfg(feature = "serde")]
pub fn to_string_pretty<T>(value: &T) -> Result<String, data_text_core::TextError>
where
    T: serde::Serialize,
{
    data_text_von::to_string_indented(value, 4)
}
