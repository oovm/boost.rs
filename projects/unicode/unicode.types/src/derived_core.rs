use crate::tables::derived_core::{DEFAULT_IGNORABLE, GRAPHEME_EXTEND};

/// 在按码点升序、互不重叠的区间表里做二分查找。
fn contains(runs: &[(u32, u32)], code_point: u32) -> bool {
    let mut low = 0usize;
    let mut high = runs.len();
    while low < high {
        let mid = (low + high) / 2;
        let (start, end) = runs[mid];
        if code_point < start {
            high = mid;
        }
        else if code_point > end {
            low = mid + 1;
        }
        else {
            return true;
        }
    }
    false
}

/// 判断标量是否具有 `Grapheme_Extend` 属性。
///
/// 该属性是 UAX #44 的派生属性，由 `DerivedCoreProperties.txt` 给出：除了 `Me` / `Mn`
/// 之外，还包含少量 `Mc` 与用于组合的其它码点。
pub fn is_grapheme_extend(ch: char) -> bool {
    contains(GRAPHEME_EXTEND, ch as u32)
}

/// 判断标量是否具有 `Default_Ignorable_Code_Point` 属性。
///
/// 覆盖 `Cf` 的一部分、变体选择符、Hangul 填充字母等「默认不可见」码点；它不等于
/// `Cf` 全集，`Cf` 里仍有需要占位的字符。
pub fn is_default_ignorable(ch: char) -> bool {
    contains(DEFAULT_IGNORABLE, ch as u32)
}
