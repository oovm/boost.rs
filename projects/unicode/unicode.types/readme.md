# `unicode-types`

Unicode 字符属性：完整标量范围的 General_Category 查询与 Mark 判定，登记为 unicode.types

Unicode 属性数据底座。

本 crate 是 `unicode.types` 能力域的首个实现：持有入库的 UCD 版本身份，提供
完整标量范围（`U+0000..=U+10FFFF`，含非 BMP）的 [`GeneralCategory`] 查询、
[`is_mark`] 判定，以及由 `DerivedCoreProperties.txt` 派生的 [`is_grapheme_extend`]
与 [`is_default_ignorable`]。数据表由 `tools/generate_general_category.py` 从仓内
入库的 UCD 输入离线生成，运行期不联网。

表按码点升序、无重叠，查询走二分查找，不以线性扫描或宿主 `char::is_*`
隐含的 Unicode 版本作为语义权威。
