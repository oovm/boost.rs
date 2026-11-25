//! 目标命名投影的共享纯函数。
//!
//! 语言侧只冻结源码名与注解解析名；各目标所有者按自己的命名合同投影时，
//! 复用这里的唯一实现，禁止在某个后端另抄一份大小写规则。

/// `two_sum` → `twoSum`（camelCase 命名投影）。
///
/// 按 `_` 分段处理，规则以段为单位而不是逐字符拼装：
/// - 首段原样保留（含已驼峰输入，无下划线时原样返回）；
/// - 后续段首字母大写，其余字符原样；
/// - 连续的单字母段视为一个词，只大写该 run 的首字母，其余字母不强制大写
///   （`o_r_s` → `ors`，而不是 `ORS`）；数字段没有大小写，原样输出并切断连读；
/// - 前导/尾随/连续下划线不产生字符，但会切断单字母连读，后续段仍算新词，
///   不吞掉词边界（`a_b__c` → `aBC`）。
///
/// `subarray_bitwise_o_r_s` → `subarrayBitwiseOrs`。
pub fn snake_case_to_camel_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut first_segment_seen = false;
    let mut previous_single_letter = false;
    for segment in name.split('_') {
        if segment.is_empty() {
            previous_single_letter = false;
            continue;
        }
        if !first_segment_seen {
            out.push_str(segment);
            first_segment_seen = true;
            previous_single_letter = false;
            continue;
        }
        let mut chars = segment.chars();
        let first = chars.next().expect("non-empty segment");
        if previous_single_letter && is_single_letter_segment(segment) {
            out.push(first);
        }
        else {
            out.extend(first.to_uppercase());
        }
        out.extend(chars);
        previous_single_letter = is_single_letter_segment(segment);
    }
    out
}

fn is_single_letter_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!((chars.next(), chars.next()), (Some(ch), None) if ch.is_alphabetic())
}

/// 名字是否为合法 C 标识符（`PyInit_<module>` 等宿主符号要能在 C 中声明）。
pub fn is_c_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// `twoSum` → `two_sum`（snake_case 命名投影，CPython 导出合同使用）。
///
/// 与 [`snake_case_to_camel_case`] 互逆方向各自独立实现，规则以词边界为单位：
/// - 已经是 snake_case 的输入原样保留（幂等：`two_sum` → `two_sum`）；
/// - 小写/数字到大写字母的跳变处插入 `_` 并把大写转小写（`twoSum` → `two_sum`）；
/// - 连续大写视为一个缩略词整体，只在词尾切分（`HTTPServer` → `http_server`）；
/// - 已有下划线保留并切断词边界，不产生空段（`foo_Bar` → `foo_bar`）。
pub fn camel_case_to_snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let chars: Vec<char> = name.chars().collect();
    for (index, &ch) in chars.iter().enumerate() {
        if ch.is_uppercase() {
            let previous = index.checked_sub(1).map(|i| chars[i]);
            let next_lower = chars.get(index + 1).is_some_and(|n| n.is_lowercase());
            // 词首（串首、下划线后）不加；小写/数字后的大写是新词；
            // 大写串末尾之后的首个大写不另起词（`HTTPServer` 的 `S` 前无下划线，
            // 但 `S` 后接小写说明新词开始）。
            let boundary = !matches!(previous, None | Some('_')) && (!previous.is_some_and(|p| p.is_uppercase()) || next_lower);
            if boundary {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        }
        else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{camel_case_to_snake_case, snake_case_to_camel_case};

    #[test]
    fn snake_case_projects_to_camel_case() {
        assert_eq!(snake_case_to_camel_case("two_sum"), "twoSum");
        assert_eq!(snake_case_to_camel_case("move_zeroes"), "moveZeroes");
        assert_eq!(snake_case_to_camel_case("a"), "a");
    }

    #[test]
    fn already_camel_input_is_preserved() {
        assert_eq!(snake_case_to_camel_case("fooBar"), "fooBar");
        assert_eq!(snake_case_to_camel_case(""), "");
    }

    #[test]
    fn single_letter_runs_read_as_one_word() {
        assert_eq!(snake_case_to_camel_case("subarray_bitwise_o_r_s"), "subarrayBitwiseOrs");
        assert_eq!(snake_case_to_camel_case("o_r_s"), "oRs");
    }

    #[test]
    fn digit_segments_and_underscore_boundaries() {
        // 数字段没有大小写，原样输出并切断连读。
        assert_eq!(snake_case_to_camel_case("a_1_b"), "a1B");
        // 前导/尾随/连续下划线不产生字符，但会切断单字母连读。
        assert_eq!(snake_case_to_camel_case("a_b__c"), "aBC");
        assert_eq!(snake_case_to_camel_case("_two_sum_"), "twoSum");
    }

    #[test]
    fn camel_case_projects_to_snake_case() {
        assert_eq!(camel_case_to_snake_case("twoSum"), "two_sum");
        assert_eq!(camel_case_to_snake_case("moveZeroes"), "move_zeroes");
        assert_eq!(camel_case_to_snake_case("TwoSum"), "two_sum");
    }

    #[test]
    fn snake_case_input_is_preserved_and_acronyms_split_at_word_tail() {
        // 幂等：已 snake 输入原样保留。
        assert_eq!(camel_case_to_snake_case("two_sum"), "two_sum");
        assert_eq!(camel_case_to_snake_case(""), "");
        // 连续大写是一个缩略词，只在接小写的新词前切分。
        assert_eq!(camel_case_to_snake_case("HTTPServer"), "http_server");
        assert_eq!(camel_case_to_snake_case("foo_Bar"), "foo_bar");
        assert_eq!(camel_case_to_snake_case("a1B"), "a1_b");
    }
}
