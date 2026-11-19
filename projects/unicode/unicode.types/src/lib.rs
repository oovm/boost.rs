//! Unicode 属性数据底座。
//!
//! 本 crate 是 `unicode.types` 能力域的首个实现：持有入库的 UCD 版本身份，提供
//! 完整标量范围（`U+0000..=U+10FFFF`，含非 BMP）的 [`GeneralCategory`] 查询、
//! [`is_mark`] 判定，以及由 `DerivedCoreProperties.txt` 派生的 [`is_grapheme_extend`]
//! 与 [`is_default_ignorable`]。数据表由 `tools/generate_general_category.py` 从仓内
//! 入库的 UCD 输入离线生成，运行期不联网。
//!
//! 表按码点升序、无重叠，查询走二分查找，不以线性扫描或宿主 `char::is_*`
//! 隐含的 Unicode 版本作为语义权威。

#![warn(missing_docs)]

mod derived_core;
mod general_category;
mod tables;

pub use derived_core::{is_default_ignorable, is_grapheme_extend};
pub use general_category::{GeneralCategory, general_category, is_mark};

/// 本 crate 数据表对应的 Unicode 版本。
pub const UNICODE_VERSION: &str = "18.0.0";

/// 返回覆盖 `U+0000..=U+10FFFF` 的 General_Category 区间表。
///
/// 返回的切片按码点升序排列，区间互不重叠且无断档，用于核验表结构与生成器输出。
pub fn general_category_runs() -> &'static [(u32, u32, GeneralCategory)] {
    tables::general_category::GENERAL_CATEGORY
}

/// 返回 `Grapheme_Extend` 属性的码点区间表。
///
/// 返回的切片按码点升序排列且互不重叠，只覆盖该属性为真的码点，用于核验表结构。
pub fn grapheme_extend_runs() -> &'static [(u32, u32)] {
    tables::derived_core::GRAPHEME_EXTEND
}

/// 返回 `Default_Ignorable_Code_Point` 属性的码点区间表。
///
/// 返回的切片按码点升序排列且互不重叠，只覆盖该属性为真的码点，用于核验表结构。
pub fn default_ignorable_runs() -> &'static [(u32, u32)] {
    tables::derived_core::DEFAULT_IGNORABLE
}
