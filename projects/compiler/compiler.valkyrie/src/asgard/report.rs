//! 报告图表的数据面：hydrated 图表项与 VX 初始化字面量。
//!
//! 数据由构建/测试/覆盖/基准报告产生，UI 在 AWSL + `asgard.plotter`；本模块只做数据建模与
//! 字面量编码，不渲染、不读工程文件。

use crate::asgard::{
    error::{AsgardError, AsgardResult},
    report_build::encode_v_text_literal,
};

/// `interactive-col-plot` 的一列条目（生成 `series` 的元素）。
#[derive(Debug, Clone, PartialEq)]
pub struct ColSeriesItem {
    /// 稳定键（图内唯一）。
    pub key: String,
    /// 显示标签。
    pub label: String,
    /// 已经带单位的数值文本。
    pub value_text: String,
    /// 柱体填充色。
    pub fill: String,
    /// 柱体高度百分比（已保证最小可见高度）。
    pub height_pct: f64,
}

/// 一张可 hydrate 的图表岛。
#[derive(Debug, Clone, PartialEq)]
pub struct HydratedChartSpec {
    /// 图表路由（同时是 AWSL wrapper 的文件名）。
    pub route: String,
    /// 图表标题。
    pub title: String,
    /// 系列条目。
    pub series: Vec<ColSeriesItem>,
}

/// 把 `series` 渲染成具名结构字面量数组。
///
/// `element` 逐字来自 wrapper 的 `let series: [package::Type]` 声明；生成器不猜类型名，
/// 路径非法时闭合失败，不回退到匿名字面量。
pub fn series_to_init_literal(series: &[ColSeriesItem], element: &str) -> AsgardResult<String> {
    if !is_type_path(element) {
        return Err(AsgardError::msg(format!(
            "`series` element type `{element}` is not a valid `package::Type` path; declare it in the wrapper instead of letting the generator guess"
        )));
    }

    let mut out = String::from("[");
    for (index, item) in series.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        let key = encode_v_text_literal(&item.key)?;
        let label = encode_v_text_literal(&item.label)?;
        let value_text = encode_v_text_literal(&item.value_text)?;
        let fill = encode_v_text_literal(&item.fill)?;
        out.push_str(&format!(
            "{element} {{ key: {key}, label: {label}, value_text: {value_text}, fill: {fill}, height_pct: {height:.1} }}",
            height = item.height_pct
        ));
    }
    out.push(']');
    Ok(out)
}

/// 类型路径必须是 `ident` 或 `ident::ident::…`。
fn is_type_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    path.split("::").all(|segment| {
        let mut chars = segment.chars();
        match chars.next() {
            Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
            _ => return false,
        }
        chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, label: &str, height: f64) -> ColSeriesItem {
        ColSeriesItem {
            key: key.into(),
            label: label.into(),
            value_text: "1 tests".into(),
            fill: "#22c55e".into(),
            height_pct: height,
        }
    }

    #[test]
    fn emits_named_struct_literals_in_declaration_order() {
        let literal = series_to_init_literal(&[item("pass", "通过", 100.0), item("fail", "失败", 4.0)], "std::chart::ColSeriesItem")
            .expect("literal");
        assert_eq!(
            literal,
            "[std::chart::ColSeriesItem { key: \"pass\", label: \"通过\", value_text: \"1 tests\", fill: \"#22c55e\", height_pct: 100.0 }, \
             std::chart::ColSeriesItem { key: \"fail\", label: \"失败\", value_text: \"1 tests\", fill: \"#22c55e\", height_pct: 4.0 }]"
        );
    }

    #[test]
    fn rejects_an_element_path_that_is_not_a_type() {
        let error = series_to_init_literal(&[item("k", "l", 1.0)], "not a type").expect_err("invalid path must close");
        assert!(error.to_string().contains("not a valid"), "{}", error);
    }

    #[test]
    fn escapes_braces_in_labels() {
        let literal = series_to_init_literal(&[item("k", "{x}", 1.0)], "T").expect("literal");
        assert!(literal.contains(r#"label: "\{x\}""#), "{literal}");
    }
}
