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

#[cfg(test)]
mod tests {
    use super::snake_case_to_camel_case;

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
}
