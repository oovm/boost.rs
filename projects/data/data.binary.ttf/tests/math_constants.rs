//! `MathConstants` conformance against MS-OTTO §3.1.
//!
//! Two independent sources are used so the parser cannot self-certify:
//! 1. a synthetic 214-byte record with known sequential values,
//! 2. Latin Modern Math (OTF/CFF, MATH v1) and Cambria Math (TrueType `glyf`
//!    inside `cambria.ttc`, face 1) checked against values read independently
//!    with fontTools.
//!
//! Font assertions are skipped with an explicit message when no font is
//! available (see `common`).

mod common;

use data_binary_ttf::{
    font::FontFace,
    reader::Reader,
    sfnt::parse_font_validated,
    tables::math::{MathConstants, MathTable},
};

fn parse_math(data: &[u8], face_index: usize) -> MathTable {
    let container = parse_font_validated(data).expect("container");
    let face = container.face(face_index).expect("face index").clone();
    let mut font = FontFace::from_face(face);
    let math = font.math().expect("MATH table").clone();
    math
}

/// A `MathConstants` record with distinguishable values for every field.
fn synthetic_constants() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(MathConstants::BYTE_LEN);
    bytes.extend_from_slice(&700i16.to_be_bytes()); // ScriptPercentScaleDown
    bytes.extend_from_slice(&500i16.to_be_bytes()); // ScriptScriptPercentScaleDown
    bytes.extend_from_slice(&1300u16.to_be_bytes()); // DelimitedSubFormulaMinHeight
    bytes.extend_from_slice(&1400u16.to_be_bytes()); // DisplayOperatorMinHeight
    for i in 1..=51i16 {
        bytes.extend_from_slice(&(i * 10).to_be_bytes()); // MathValueRecord value
        bytes.extend_from_slice(&0u16.to_be_bytes()); // deviceTableOffset
    }
    bytes.extend_from_slice(&60u16.to_be_bytes()); // RadicalDegreeBottomRaisePercent
    bytes
}

#[test]
fn constants_layout_follows_specification_order() {
    let bytes = synthetic_constants();
    assert_eq!(bytes.len(), MathConstants::BYTE_LEN);

    let mut reader = Reader::new(&bytes);
    let c = MathConstants::parse(&mut reader).expect("parse synthetic constants");
    assert_eq!(reader.position(), MathConstants::BYTE_LEN, "exactly 214 bytes are consumed");

    // 4 simple fields
    assert_eq!(c.script_percent_scale_down, 700.0);
    assert_eq!(c.script_script_percent_scale_down, 500.0);
    assert_eq!(c.delimited_sub_formula_min_height, 1300.0);
    assert_eq!(c.display_operator_min_height, 1400.0);

    // MathValueRecord #n (1-based) carries the value n * 10.
    assert_eq!(c.math_leading, 10.0, "record 1");
    assert_eq!(c.axis_height, 20.0, "record 2");
    assert_eq!(c.superscript_baseline_drop_max, 110.0, "record 11");
    assert_eq!(c.space_after_script, 140.0, "record 14: SpaceAfterScript follows SuperscriptBottomMaxWithSubscript");
    assert_eq!(c.stack_gap_min, 230.0, "record 23");
    assert_eq!(c.fraction_rule_thickness, 350.0, "record 35");
    assert_eq!(c.radical_vertical_gap, 460.0, "record 46");
    assert_eq!(c.radical_kern_after_degree, 510.0, "record 51");
    assert_eq!(c.radical_degree_bottom_raise_percent, 60.0);
}

#[test]
fn constants_reject_truncated_record() {
    let mut bytes = synthetic_constants();
    bytes.truncate(MathConstants::BYTE_LEN - 2); // drop the trailing uint16
    let mut reader = Reader::new(&bytes);
    assert!(MathConstants::parse(&mut reader).is_err(), "a truncated record must not parse");
}

#[test]
fn constants_match_specification_on_latin_modern_math() {
    let Some(data) = common::latin_modern_math()
    else {
        return;
    };
    let our_math = parse_math(&data, 0);

    // Independent specification values for Latin Modern Math.
    let c = &our_math.constants;
    assert_eq!(c.script_percent_scale_down, 70.0);
    assert_eq!(c.script_script_percent_scale_down, 50.0);
    assert_eq!(c.axis_height, 250.0);
    assert!(c.axis_height > 0.0);
    assert_eq!(c.space_after_script, 56.0);
    assert_eq!(c.radical_degree_bottom_raise_percent, 60.0);
}

#[test]
fn constants_match_specification_on_cambria_math() {
    let Some(data) = common::cambria_math()
    else {
        return;
    };
    // Face 0 is Cambria, face 1 is Cambria Math (TrueType `glyf` outlines).
    let our_math = parse_math(&data, 1);

    let c = &our_math.constants;
    assert_eq!(c.script_percent_scale_down, 73.0);
    assert_eq!(c.axis_height, 585.0);
    assert_eq!(c.space_after_script, 85.0);
    assert_eq!(c.radical_degree_bottom_raise_percent, 65.0);
}
