use unicode_width::{AmbiguousWidth, EastAsianWidth, WidthProfile, char_width, char_width_with, east_asian_width, str_width, str_width_with};

fn wide() -> WidthProfile {
    WidthProfile { ambiguous: AmbiguousWidth::Wide }
}

#[test]
fn ascii_printable_is_single_width_and_control_is_none() {
    for code in 0x20u32..=0x7E {
        let ch = char::from_u32(code).unwrap();
        assert_eq!(char_width(ch), Some(1), "U+{:04X}", code);
    }
    assert_eq!(char_width('\n'), None);
    assert_eq!(char_width('\t'), None);
    assert_eq!(char_width('\0'), None);
    assert_eq!(char_width('\u{7F}'), None);
    assert_eq!(char_width('\u{9F}'), None);
}

#[test]
fn cjk_wide_fullwidth_and_halfwidth() {
    assert_eq!(char_width('\u{4E00}'), Some(2));
    assert_eq!(char_width('\u{65E5}'), Some(2));
    assert_eq!(char_width('\u{FF01}'), Some(2));
    assert_eq!(char_width('\u{FF61}'), Some(1));
    assert_eq!(char_width('\u{3000}'), Some(2));
    assert_eq!(east_asian_width('\u{4E00}'), EastAsianWidth::Wide);
    assert_eq!(east_asian_width('\u{FF01}'), EastAsianWidth::Fullwidth);
    assert_eq!(east_asian_width('\u{FF61}'), EastAsianWidth::Halfwidth);
}

#[test]
fn ambiguous_follows_profile() {
    assert_eq!(WidthProfile::default(), WidthProfile { ambiguous: AmbiguousWidth::Narrow });
    assert_eq!(east_asian_width('\u{A1}'), EastAsianWidth::Ambiguous);
    assert_eq!(char_width('\u{A1}'), Some(1));
    assert_eq!(char_width_with('\u{A1}', wide()), Some(2));
    // 默认 profile 与显式 `Narrow` 等价，`Wide` 只影响 `A` 类字符。
    assert_eq!(char_width_with('\u{A1}', WidthProfile { ambiguous: AmbiguousWidth::Narrow }), Some(1));
    assert_eq!(char_width_with('\u{4E00}', wide()), Some(2));
    assert_eq!(str_width_with("\u{A1}ab", wide()), 4);
    assert_eq!(str_width_with("\u{3042}\u{A1}", wide()), 4);
}

#[test]
fn combining_marks_are_zero_width() {
    for ch in ['\u{301}', '\u{483}', '\u{64B}', '\u{1D165}'] {
        assert_eq!(char_width(ch), Some(0), "U+{:04X}", ch as u32);
    }
}

#[test]
fn format_characters_are_zero_width() {
    assert_eq!(char_width('\u{200B}'), Some(0));
    assert_eq!(char_width('\u{200D}'), Some(0));
    assert_eq!(char_width('\u{AD}'), Some(0));
    assert_eq!(char_width('\u{FE0F}'), Some(0));
}

#[test]
fn emoji_presentation_is_double_width() {
    assert_eq!(char_width('\u{1F600}'), Some(2));
    assert_eq!(str_width("\u{1F600}"), 2);
    assert_eq!(str_width("a\u{1F600}b"), 4);
    // 逐字符求和的口径：变体选择符本身 0 宽，序列整体宽度由渲染层决定。
    assert_eq!(str_width("\u{2764}\u{FE0F}"), 1);
}

#[test]
fn run_table_covers_scalars_without_gap() {
    let runs = unicode_width::east_asian_width_runs();
    assert!(!runs.is_empty());
    assert_eq!(runs[0].0, 0);
    assert_eq!(runs[runs.len() - 1].1, 0x10FFFF);
    for run in runs {
        assert!(run.0 <= run.1, "inverted run at U+{:04X}", run.0);
    }
    for pair in runs.windows(2) {
        assert_eq!(pair[0].1 + 1, pair[1].0, "gap or overlap at U+{:04X}", pair[1].0);
        assert_ne!(pair[0].2, pair[1].2, "unmerged run at U+{:04X}", pair[1].0);
    }

    // 抽样 run 边界：边界两侧的码点必须归入不同的属性。
    let mut checked = 0usize;
    for run in runs.iter().skip(1).step_by(37) {
        let (Some(previous), Some(next)) = (char::from_u32(run.0 - 1), char::from_u32(run.0))
        else {
            continue;
        };
        assert_ne!(east_asian_width(previous), east_asian_width(next), "boundary U+{:04X}", run.0);
        checked += 1;
    }
    assert!(checked >= 10, "only {checked} boundaries checked");
}

/// 宽度快照：期望值取自切换前 `unicode-width 0.2.3` 对同一字符的输出。
///
/// 这些字符覆盖 ASCII、控制字符、CJK、半角/全角、组合符、`Cf`、emoji 与非 BMP。
#[test]
fn width_snapshot_matches_previous_implementation() {
    let cases: &[(char, Option<usize>)] = &[
        (' ', Some(1)),
        ('0', Some(1)),
        ('~', Some(1)),
        ('\u{7F}', None),
        ('\u{80}', None),
        ('\u{A0}', Some(1)),
        ('\u{A1}', Some(1)),
        ('\u{A2}', Some(1)),
        ('\u{AD}', Some(0)),
        ('\u{300}', Some(0)),
        ('\u{301}', Some(0)),
        ('\u{483}', Some(0)),
        ('\u{64B}', Some(0)),
        ('\u{900}', Some(0)),
        ('\u{1D165}', Some(0)),
        ('\u{200B}', Some(0)),
        ('\u{200D}', Some(0)),
        ('\u{FE0F}', Some(0)),
        ('\u{20A9}', Some(1)),
        ('\u{2500}', Some(1)),
        ('\u{2600}', Some(1)),
        ('\u{1D100}', Some(1)),
        ('\u{1D400}', Some(1)),
        ('\u{1D7CE}', Some(1)),
        ('\u{3000}', Some(2)),
        ('\u{4E00}', Some(2)),
        ('\u{65E5}', Some(2)),
        ('\u{9FFF}', Some(2)),
        ('\u{FF01}', Some(2)),
        ('\u{FF61}', Some(1)),
        ('\u{231A}', Some(2)),
        ('\u{2614}', Some(2)),
        ('\u{2705}', Some(2)),
        ('\u{1F1E6}', Some(1)),
        ('\u{1F3FB}', Some(2)),
        ('\u{1F600}', Some(2)),
        ('\u{1F9FF}', Some(2)),
        ('\u{E000}', Some(1)),
        ('\u{FFFD}', Some(1)),
        ('\u{378}', Some(1)),
        ('\u{20000}', Some(2)),
        ('\u{2A6E0}', Some(2)),
        ('\u{3134A}', Some(2)),
        ('\u{10FFFF}', Some(1)),
    ];
    for (ch, expected) in cases {
        assert_eq!(char_width(*ch), *expected, "U+{:04X}", *ch as u32);
    }
}

/// 与 `unicode-width 0.2.3` 曾不一致、现已对齐的字符。
///
/// 旧库的零宽集合是 `Grapheme_Extend` ∪ `Default_Ignorable_Code_Point` ∪ Hangul 组合字母
/// `V`/`T` ∪ 少量特例；本 crate 升级到 Unicode 18.0.0 后改为同一口径，并保留旧库对
/// `U+17D8`、`U+17A4`、`U+A8FA`、`U+3164` 等标量的显式取值。
#[test]
fn previously_divergent_scalars_now_match_previous_implementation() {
    // Mark 类里只有 `Grapheme_Extend` 才是 0 宽：`U+0903`（`Mc`）不在该属性内，取 1。
    assert_eq!(char_width('\u{903}'), Some(1));
    // `Cf` 里只有 `Default_Ignorable_Code_Point` 才是 0 宽：`U+0600` 取 1。
    assert_eq!(char_width('\u{600}'), Some(1));
    // Hangul 组合字母（`Lo`，`East_Asian_Width` 为 `N`）按 0 宽处理。
    assert_eq!(char_width('\u{1160}'), Some(0));
    assert_eq!(char_width('\u{11A8}'), Some(0));
    assert_eq!(char_width('\u{D7B0}'), Some(0));
    assert_eq!(char_width('\u{D7FB}'), Some(0));
    // 旧库写死的宽度特例。
    assert_eq!(char_width('\u{17D8}'), Some(3));
    assert_eq!(char_width('\u{17A4}'), Some(2));
    assert_eq!(char_width('\u{A8FA}'), Some(0));
    assert_eq!(char_width('\u{3164}'), Some(0));
    assert_eq!(char_width('\u{115F}'), Some(2));
    assert_eq!(char_width('\u{FFA0}'), Some(0));
    // `Grapheme_Extend` 里的两个例外与 Tifinagh 连接符按 1 列计。
    assert_eq!(char_width('\u{2D7F}'), Some(1));
    assert_eq!(char_width('\u{FF9E}'), Some(1));
    assert_eq!(char_width('\u{FF9F}'), Some(1));
    // Unicode 18.0.0 新增的 `Mn`：落在 `Grapheme_Extend` 区间内，取 0。
    assert_eq!(char_width('\u{1ACF}'), Some(0));
}
