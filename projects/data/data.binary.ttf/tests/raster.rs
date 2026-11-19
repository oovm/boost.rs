//! Rasterizer tests: a synthetic TrueType font that runs everywhere, plus
//! real system-font checks that are `#[ignore]`d because they need Windows.
//!
//! The synthetic font is assembled by hand (SFNT directory, `head`, `hhea`,
//! `maxp`, `cmap`, `hmtx`, `loca`, `glyf`) so the default suite has one known
//! square glyph without depending on a font binary.

use data_binary_ttf::raster::Font;

// ---- synthetic TrueType font with one 800x800 square ----

fn be16(value: u16) -> [u8; 2] {
    value.to_be_bytes()
}

fn be32(value: u32) -> [u8; 4] {
    value.to_be_bytes()
}

fn push_u16(buffer: &mut Vec<u8>, value: u16) {
    buffer.extend_from_slice(&be16(value));
}

fn push_i16(buffer: &mut Vec<u8>, value: i16) {
    buffer.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&be32(value));
}

/// `head` with long `loca` offsets and a 1000-unit em.
fn head_table() -> Vec<u8> {
    let mut table = Vec::with_capacity(54);
    push_u32(&mut table, 0x0001_0000); // version
    push_u32(&mut table, 0x0001_0000); // fontRevision
    push_u32(&mut table, 0); // checkSumAdjustment
    push_u32(&mut table, 0x5F0F_3CF5); // magicNumber
    push_u16(&mut table, 0); // flags
    push_u16(&mut table, 1000); // unitsPerEm
    table.extend_from_slice(&0i64.to_be_bytes()); // created
    table.extend_from_slice(&0i64.to_be_bytes()); // modified
    push_i16(&mut table, 100); // xMin
    push_i16(&mut table, 0); // yMin
    push_i16(&mut table, 900); // xMax
    push_i16(&mut table, 800); // yMax
    push_u16(&mut table, 0); // macStyle
    push_u16(&mut table, 8); // lowestRecPPEM
    push_i16(&mut table, 2); // fontDirectionHint
    push_i16(&mut table, 1); // indexToLocFormat: long
    push_i16(&mut table, 0); // glyphDataFormat
    table
}

fn hhea_table() -> Vec<u8> {
    let mut table = Vec::with_capacity(36);
    push_u32(&mut table, 0x0001_0000);
    push_i16(&mut table, 800); // ascender
    push_i16(&mut table, -200); // descender
    push_i16(&mut table, 0); // lineGap
    push_u16(&mut table, 500); // advanceWidthMax
    push_i16(&mut table, 0); // minLeftSideBearing
    push_i16(&mut table, 0); // minRightSideBearing
    push_i16(&mut table, 0); // xMaxExtent
    push_i16(&mut table, 0); // caretSlopeRise
    push_i16(&mut table, 0); // caretSlopeRun
    push_i16(&mut table, 0); // caretOffset
    for _ in 0..4 {
        push_i16(&mut table, 0); // reserved
    }
    push_i16(&mut table, 0); // metricDataFormat
    push_u16(&mut table, 2); // numberOfHMetrics
    table
}

fn maxp_table() -> Vec<u8> {
    let mut table = Vec::with_capacity(6);
    push_u32(&mut table, 0x0000_5000); // version 0.5
    push_u16(&mut table, 2); // numGlyphs: .notdef + square
    table
}

/// Format 4 cmap mapping `A` (U+0041) to glyph 1.
fn cmap_table() -> Vec<u8> {
    let mut subtable = Vec::new();
    push_u16(&mut subtable, 4); // format
    push_u16(&mut subtable, 32); // length
    push_u16(&mut subtable, 0); // language
    push_u16(&mut subtable, 4); // segCountX2
    push_u16(&mut subtable, 4); // searchRange
    push_u16(&mut subtable, 1); // entrySelector
    push_u16(&mut subtable, 0); // rangeShift
    push_u16(&mut subtable, 0x41); // endCode[0]
    push_u16(&mut subtable, 0xFFFF); // endCode[1]
    push_u16(&mut subtable, 0); // reservedPad
    push_u16(&mut subtable, 0x41); // startCode[0]
    push_u16(&mut subtable, 0xFFFF); // startCode[1]
    push_u16(&mut subtable, (-64i16) as u16); // idDelta[0]: 0x41 - 64 = 1
    push_u16(&mut subtable, 1); // idDelta[1]
    push_u16(&mut subtable, 0); // idRangeOffset[0]
    push_u16(&mut subtable, 0); // idRangeOffset[1]

    let mut table = Vec::new();
    push_u16(&mut table, 0); // version
    push_u16(&mut table, 1); // numTables
    push_u16(&mut table, 3); // platform: Windows
    push_u16(&mut table, 1); // encoding: Unicode BMP
    push_u32(&mut table, 12); // subtable offset
    table.extend_from_slice(&subtable);
    table
}

fn hmtx_table() -> Vec<u8> {
    let mut table = Vec::new();
    push_u16(&mut table, 500); // .notdef advance
    push_i16(&mut table, 0);
    push_u16(&mut table, 500); // square advance
    push_i16(&mut table, 100);
    table
}

/// One on-curve square contour: (100,0) (900,0) (900,800) (100,800).
fn square_glyph() -> Vec<u8> {
    let mut glyph = Vec::new();
    push_i16(&mut glyph, 1); // numberOfContours
    push_i16(&mut glyph, 100); // xMin
    push_i16(&mut glyph, 0); // yMin
    push_i16(&mut glyph, 900); // xMax
    push_i16(&mut glyph, 800); // yMax
    push_u16(&mut glyph, 3); // endPtsOfContours
    push_u16(&mut glyph, 0); // instructionLength
    glyph.extend_from_slice(&[0x01; 4]); // ON_CURVE, 16-bit deltas
    for delta in [100i16, 800, 0, -800] {
        push_i16(&mut glyph, delta); // x deltas
    }
    for delta in [0i16, 0, 800, 0] {
        push_i16(&mut glyph, delta); // y deltas
    }
    glyph
}

fn synthetic_font() -> Vec<u8> {
    let glyph = square_glyph();
    let mut loca = Vec::new();
    push_u32(&mut loca, 0); // glyph 0 starts at 0 and is empty
    push_u32(&mut loca, 0);
    push_u32(&mut loca, glyph.len() as u32);

    let tables: [(&[u8; 4], Vec<u8>); 7] = [
        (b"head", head_table()),
        (b"hhea", hhea_table()),
        (b"maxp", maxp_table()),
        (b"cmap", cmap_table()),
        (b"hmtx", hmtx_table()),
        (b"loca", loca),
        (b"glyf", glyph),
    ];

    let count = tables.len() as u16;
    let mut font = Vec::new();
    push_u32(&mut font, 0x0001_0000); // SFNT version
    push_u16(&mut font, count);
    push_u16(&mut font, 0); // searchRange (informational)
    push_u16(&mut font, 0); // entrySelector
    push_u16(&mut font, 0); // rangeShift

    let mut offset = 12 + 16 * tables.len();
    let mut records = Vec::new();
    let mut data = Vec::new();
    for (tag, table) in &tables {
        records.extend_from_slice(*tag);
        push_u32(&mut records, 0); // checksum (the rasterizer does not verify)
        push_u32(&mut records, offset as u32);
        push_u32(&mut records, table.len() as u32);
        data.extend_from_slice(table);
        let padded = (table.len() + 3) & !3;
        data.resize(data.len() + padded - table.len(), 0);
        offset += padded;
    }
    font.extend_from_slice(&records);
    font.extend_from_slice(&data);
    font
}

#[test]
fn synthetic_square_rasterizes() {
    let mut font = Font::from_bytes(synthetic_font(), 0).expect("synthetic font parses");
    assert!(font.has_glyph('A'));
    assert!(!font.has_glyph('B'));

    let (metrics, bitmap) = font.rasterize('A', 100.0);
    assert_eq!((metrics.xmin, metrics.ymin), (10, 0), "100..900 font units at size 100");
    assert_eq!((metrics.width, metrics.height), (80, 80));
    assert_eq!(bitmap.len(), metrics.width * metrics.height);
    assert!((metrics.advance_width - 50.0).abs() < 1e-4, "advance={}", metrics.advance_width);

    let at = |column: usize, row: usize| bitmap[row * metrics.width + column];
    for (column, row) in [(0, 0), (79, 0), (0, 79), (79, 79), (40, 40)] {
        assert!(at(column, row) > 0, "pixel ({column},{row}) should be inked");
    }
    assert!(bitmap.iter().all(|coverage| *coverage == 255), "the square is pixel aligned, so every covered pixel is opaque");
}

#[test]
fn missing_and_empty_glyphs_keep_advance() {
    let mut font = Font::from_bytes(synthetic_font(), 0).expect("synthetic font parses");
    // .notdef is empty but still has an advance.
    let (notdef, empty) = font.rasterize('\u{10FFFF}', 100.0);
    assert_eq!((notdef.width, notdef.height), (0, 0));
    assert!(empty.is_empty());
    assert!(notdef.advance_width > 0.0, "a missing glyph falls back to the .notdef advance");

    // A zero size must not divide by zero or allocate.
    let (zero, bitmap) = font.rasterize('A', 0.0);
    assert_eq!((zero.width, zero.height), (0, 0));
    assert!(bitmap.is_empty());
}

#[test]
fn bad_font_bytes_are_rejected_without_panicking() {
    assert!(Font::from_bytes(Vec::new(), 0).is_err());
    assert!(Font::from_bytes(vec![0x7F; 64], 0).is_err());
    let mut truncated = synthetic_font();
    truncated.truncate(20);
    assert!(Font::from_bytes(truncated, 0).is_err());
}

// ---- system fonts (explicitly requested; not part of the default run) ----

fn system_font(path: &str, index: u32) -> Font {
    let bytes = std::fs::read(path).unwrap_or_else(|error| panic!("read {path}: {error}"));
    Font::from_bytes(bytes, index).unwrap_or_else(|error| panic!("parse {path}: {error}"))
}

fn inked(bitmap: &[u8]) -> bool {
    bitmap.iter().any(|coverage| *coverage > 0)
}

#[test]
#[ignore = "requires Windows system fonts"]
fn msyh_ttc_covers_latin_cjk_and_space() {
    let mut font = system_font(r"C:\Windows\Fonts\msyh.ttc", 0);
    let (a_metrics, a_bitmap) = font.rasterize('A', 32.0);
    assert!(inked(&a_bitmap), "A produced an empty bitmap");
    assert!(a_metrics.advance_width > 0.0);
    let (cjk_metrics, cjk_bitmap) = font.rasterize('中', 32.0);
    assert!(inked(&cjk_bitmap), "中 produced an empty bitmap");
    assert!(cjk_metrics.advance_width > 0.0);
    let (space_metrics, space_bitmap) = font.rasterize(' ', 32.0);
    assert_eq!((space_metrics.width, space_metrics.height), (0, 0));
    assert!(space_bitmap.is_empty());
    assert!(space_metrics.advance_width > 0.0, "space advance={}", space_metrics.advance_width);
    eprintln!("msyh.ttc[0] @32px: A={a_metrics:?} 中={cjk_metrics:?} space={space_metrics:?}");
}

#[test]
#[ignore = "requires Windows system fonts"]
fn segoeui_composite_accent_rasterizes() {
    let mut font = system_font(r"C:\Windows\Fonts\segoeui.ttf", 0);
    let (metrics, bitmap) = font.rasterize('é', 32.0);
    assert!(inked(&bitmap), "é (a composite glyph) produced an empty bitmap");
    assert!(metrics.width > 0 && metrics.height > 0);
    assert!(metrics.advance_width > 0.0);
    eprintln!("segoeui.ttf é @32px: {metrics:?}");
}

#[test]
#[ignore = "requires Windows system fonts"]
fn latin_modern_math_cff_rasterizes() {
    let mut font = system_font(r"C:\Windows\Fonts\LatinModernMath-Regular.otf", 0);
    for ch in ['A', '∑'] {
        let (metrics, bitmap) = font.rasterize(ch, 32.0);
        assert!(inked(&bitmap), "{ch} produced an empty CFF bitmap");
        assert!(metrics.width > 0 && metrics.height > 0, "{ch} metrics={metrics:?}");
        assert!(metrics.advance_width > 0.0);
        eprintln!("LatinModernMath-Regular.otf {ch} @32px: {metrics:?}");
    }
}
