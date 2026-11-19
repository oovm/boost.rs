//! Unicode 终端列宽估计。
//!
//! 本 crate 是 `unicode.width` 能力域的实现：持有 `East_Asian_Width` 查询与显式
//! width profile，给出上下文无关的单字符列宽与逐字符求和。数据表由
//! `tools/generate_east_asian_width.py` 从仓内入库的 UCD 输入离线生成，运行期不联网。
//!
//! 表按码点升序、无重叠无断档，查询走二分查找，不以宿主 `char` 方法隐含的 Unicode
//! 版本作为语义权威。
//!
//! 这里只是终端列宽的估计：emoji ZWJ 序列、区域指示符对、变体选择符的组合宽度以及
//! 字体的实际度量由渲染层负责，本 crate 不承诺这些序列的整体列宽。

#![warn(missing_docs)]

mod east_asian_width;
mod tables;
mod width;

pub use east_asian_width::{EastAsianWidth, east_asian_width};
pub use unicode_types::UNICODE_VERSION;
pub use width::{AmbiguousWidth, WidthProfile, char_width, char_width_with, str_width, str_width_with};

/// 返回覆盖 `U+0000..=U+10FFFF` 的 East_Asian_Width 区间表。
///
/// 返回的切片按码点升序排列，区间互不重叠且无断档，用于核验表结构与生成器输出。
pub fn east_asian_width_runs() -> &'static [(u32, u32, EastAsianWidth)] {
    tables::east_asian_width::EAST_ASIAN_WIDTH
}
