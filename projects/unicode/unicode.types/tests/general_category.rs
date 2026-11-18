//! `unicode-types` 的 General_Category 查询与区间表结构验收。

use unicode_types::{
    GeneralCategory, UNICODE_VERSION, default_ignorable_runs, general_category, general_category_runs, grapheme_extend_runs,
    is_default_ignorable, is_grapheme_extend, is_mark,
};

const MAX_CODE_POINT: u32 = 0x10FFFF;

fn category_of(code_point: u32) -> GeneralCategory {
    let ch = char::from_u32(code_point).unwrap_or_else(|| panic!("U+{code_point:04X} 不是标量"));
    general_category(ch)
}

fn mark_of(code_point: u32) -> bool {
    let ch = char::from_u32(code_point).unwrap_or_else(|| panic!("U+{code_point:04X} 不是标量"));
    is_mark(ch)
}

#[test]
fn ascii_categories() {
    assert_eq!(general_category('A'), GeneralCategory::Lu);
    assert_eq!(general_category('Z'), GeneralCategory::Lu);
    assert_eq!(general_category('a'), GeneralCategory::Ll);
    assert_eq!(general_category('z'), GeneralCategory::Ll);
    assert_eq!(general_category('0'), GeneralCategory::Nd);
    assert_eq!(general_category('9'), GeneralCategory::Nd);
    assert_eq!(general_category(' '), GeneralCategory::Zs);
    assert_eq!(general_category('!'), GeneralCategory::Po);
    assert_eq!(general_category('('), GeneralCategory::Ps);
    assert_eq!(general_category(')'), GeneralCategory::Pe);
    assert_eq!(general_category('-'), GeneralCategory::Pd);
    assert_eq!(general_category('_'), GeneralCategory::Pc);
    assert_eq!(general_category('+'), GeneralCategory::Sm);
    assert_eq!(general_category('$'), GeneralCategory::Sc);
}

#[test]
fn legacy_gap_marks_are_marks() {
    // 这些 Mark 不在旧手写五段 combining 区间内，或只被部分覆盖。
    for code_point in [0x0483, 0x0591, 0x064B, 0x1AB0, 0x20D0, 0xFE20] {
        assert!(mark_of(code_point), "U+{code_point:04X} 应为 Mark");
    }
    assert_eq!(category_of(0x0483), GeneralCategory::Mn);
    assert_eq!(category_of(0x0591), GeneralCategory::Mn);
    assert_eq!(category_of(0x064B), GeneralCategory::Mn);
    assert_eq!(category_of(0x1AB0), GeneralCategory::Mn);
    assert_eq!(category_of(0x20D0), GeneralCategory::Mn);
    assert_eq!(category_of(0xFE20), GeneralCategory::Mn);
}

#[test]
fn non_bmp_and_non_mark_scalars() {
    assert_eq!(category_of(0x1D165), GeneralCategory::Mc);
    assert!(mark_of(0x1D165));
    assert_eq!(category_of(0x1F600), GeneralCategory::So);
    assert!(!mark_of(0x1F600));
    assert_eq!(category_of(0xAC00), GeneralCategory::Lo);
    assert!(!mark_of(0xAC00));
    // 常见拉丁组合符与希伯来点仍是 Mark，确认没有把 Mark 判定退化成区间特例。
    assert!(mark_of(0x0301));
    assert!(mark_of(0x05B0));
}

#[test]
fn runs_cover_the_code_point_space_without_gaps_or_overlap() {
    let runs = general_category_runs();
    assert!(runs.len() > 1, "区间表不应为空");
    assert_eq!(runs[0].0, 0, "首区间必须始于 U+0000");
    assert_eq!(runs[runs.len() - 1].1, MAX_CODE_POINT, "末区间必须止于 U+10FFFF");

    for (index, run) in runs.iter().enumerate() {
        let (start, end, _) = *run;
        assert!(start <= end, "区间 #{index} 起点晚于终点");
        if let Some(next) = runs.get(index + 1) {
            assert_eq!(next.0, end + 1, "区间 #{index} 与下一区间之间出现断档或重叠");
            assert!(next.2 != run.2, "区间 #{index} 与下一区间类别相同，说明生成器没有做 run-length 合并");
        }
    }
}

#[test]
fn run_boundaries_resolve_to_their_own_category() {
    for (index, run) in general_category_runs().iter().enumerate() {
        let (start, end, category) = *run;
        for code_point in [start, end] {
            if let Some(ch) = char::from_u32(code_point) {
                assert_eq!(general_category(ch), category, "区间 #{index} 的边界 U+{code_point:04X} 落到了别的类别");
            }
        }
        if let Some(next) = general_category_runs().get(index + 1)
            && let Some(ch) = char::from_u32(next.0)
        {
            assert_ne!(general_category(ch), category, "区间 #{index} 的下一区间起点 U+{:04X} 与上一区间类别相同", next.0);
        }
    }
}

#[test]
fn unicode_version_matches_manifest() {
    assert_eq!(UNICODE_VERSION, "18.0.0");
}

#[test]
fn derived_properties_match_ucd_facts() {
    // `Grapheme_Extend` 覆盖全部 `Mn` / `Me`，并只覆盖一部分 `Mc`。
    assert!(is_grapheme_extend('\u{301}'));
    assert!(is_grapheme_extend('\u{483}'));
    assert!(is_grapheme_extend('\u{1D165}'));
    assert!(!is_grapheme_extend('\u{903}'));
    assert!(!is_grapheme_extend('\u{4E00}'));
    // `Default_Ignorable_Code_Point` 不等于 `Cf` 全集。
    assert!(is_default_ignorable('\u{AD}'));
    assert!(is_default_ignorable('\u{200D}'));
    assert!(is_default_ignorable('\u{FE0F}'));
    assert!(is_default_ignorable('\u{3164}'));
    assert!(!is_default_ignorable('\u{600}'));
    assert!(!is_default_ignorable('\u{4E00}'));
}

#[test]
fn derived_property_runs_are_sorted_and_disjoint() {
    fn check(runs: &[(u32, u32)], inside: fn(char) -> bool) {
        assert!(!runs.is_empty(), "派生属性区间表不应为空");
        for (index, (start, end)) in runs.iter().enumerate() {
            assert!(start <= end, "区间 #{index} 起点晚于终点");
            if let Some((next_start, _)) = runs.get(index + 1) {
                assert!(end + 1 < *next_start, "区间 #{index} 与下一区间重叠或未合并");
            }
            for code_point in [*start, *end] {
                let ch = char::from_u32(code_point).expect("派生属性的边界必须是标量");
                assert!(inside(ch), "区间边界 U+{code_point:04X} 未落在属性内");
            }
        }
    }
    check(grapheme_extend_runs(), is_grapheme_extend);
    check(default_ignorable_runs(), is_default_ignorable);
}

#[test]
fn category_spellings_round_trip() {
    for (category, spelling) in [
        (GeneralCategory::Lu, "Lu"),
        (GeneralCategory::Mn, "Mn"),
        (GeneralCategory::Mc, "Mc"),
        (GeneralCategory::Me, "Me"),
        (GeneralCategory::Zs, "Zs"),
        (GeneralCategory::Cn, "Cn"),
    ] {
        assert_eq!(category.as_str(), spelling);
    }
    assert_eq!(GeneralCategory::Lu.as_str().len(), 2);
}
