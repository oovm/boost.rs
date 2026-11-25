//! Deck-level size/title, shape style and table grid edit tests.
//!
//! These cover the operations the product used to defer outside the transaction:
//! deck title and extent, shape fill and stroke, table column widths, row heights,
//! cell fills, and row/column re-seating and duplication. Every edit must stay
//! inside the atomic draft, keep identities, and survive a project round trip.

use document_presentation::authored::{
    Deck, DeckCreateOptions, DeckError, DeckSize, ElementContent, ElementId, Fill, Line, ShapeElementInput, SlideAddOptions, SlideId,
    TableCell, TableElementInput, TableRow, TextContent, TextElementInput, Transform, codes, decode_project, encode_project, points_to_emu,
};

fn deck() -> Deck {
    Deck::create(DeckCreateOptions::default())
}

fn frame() -> Transform {
    Transform::from_points(40.0, 120.0, 400.0, 200.0).expect("valid box")
}

fn emu(points: f64) -> i64 {
    points_to_emu(points).expect("positive points")
}

/// A deck with one slide carrying one text element, one shape and one 2×2 table.
fn sample() -> (Deck, SlideId, ElementId, ElementId) {
    let mut deck = deck();
    let (slide, shape, table) = deck
        .transaction(None, |edit| {
            let slide = edit.add_slide(SlideAddOptions::default())?;
            edit.add_text(
                &slide,
                TextElementInput { text: TextContent::plain("body"), transform: Some(frame()), alt: None, placeholder: None },
            )?;
            let shape = edit.add_shape(
                &slide,
                ShapeElementInput { geometry: "rect".to_string(), transform: None, fill: None, line: None, alt: None },
            )?;
            let table = edit.add_table(
                &slide,
                TableElementInput {
                    column_widths_emu: vec![emu(80.0), emu(120.0)],
                    rows: vec![
                        TableRow { height_emu: emu(24.0), cells: vec![TableCell::plain("A"), TableCell::plain("B")] },
                        TableRow { height_emu: emu(24.0), cells: vec![TableCell::plain("1"), TableCell::plain("2")] },
                    ],
                    transform: None,
                    alt: Some("grid".to_string()),
                },
            )?;
            Ok((slide, shape, table))
        })
        .expect("sample deck");
    (deck, slide, shape, table)
}

fn table_rows(deck: &Deck, slide: &SlideId, element: &ElementId) -> (Vec<i64>, Vec<Vec<String>>, Vec<Vec<Option<Fill>>>) {
    let model = deck.slide(slide).expect("slide");
    let target = model.element(element).expect("table");
    let ElementContent::Table(table) = &target.content else {
        panic!("the element is a table");
    };
    let heights = table.rows.iter().map(|row| row.height_emu).collect();
    let texts = table
        .rows
        .iter()
        .map(|row| row.cells.iter().map(|cell| cell.text.text()).collect::<Vec<_>>())
        .collect();
    let fills = table.rows.iter().map(|row| row.cells.iter().map(|cell| cell.fill.clone()).collect::<Vec<_>>()).collect();
    (heights, texts, fills)
}

#[test]
fn deck_title_and_extent_survive_a_project_roundtrip() {
    let (mut deck, _, _, _) = sample();
    let size = DeckSize::from_points(1000.0, 562.5).expect("custom extent");

    deck.transaction(None, |edit| {
        edit.set_deck_title(Some("  季度复盘  ".to_string()))?;
        edit.set_deck_size(size)
    })
    .expect("deck level edits");

    assert_eq!(deck.title(), Some("季度复盘"), "the accepted title is trimmed");
    assert_eq!(deck.size(), size);

    let bytes = encode_project(&deck).expect("encode");
    let mut reopened = decode_project(&bytes).expect("decode");
    assert_eq!(reopened.title(), Some("季度复盘"));
    assert_eq!(reopened.size(), size);
    assert_eq!(encode_project(&reopened).expect("re-encode"), bytes);

    reopened.transaction(None, |edit| edit.set_deck_title(None)).expect("clear title");
    assert_eq!(reopened.title(), None);
    assert!(reopened.size() == size, "clearing the title never touches the extent");
}

#[test]
fn deck_title_and_extent_reject_unusable_values_atomically() {
    let (mut deck, _, _, _) = sample();
    let revision = deck.revision();
    let before = deck.size();

    let error = deck
        .transaction::<()>(None, |edit| edit.set_deck_title(Some("   ".to_string())))
        .expect_err("a blank title is rejected");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    let error = deck
        .transaction::<()>(None, |edit| edit.set_deck_size(DeckSize { width_emu: 0, height_emu: 1 }))
        .expect_err("a zero width is rejected");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);

    assert_eq!(deck.revision(), revision);
    assert_eq!(deck.title(), None);
    assert_eq!(deck.size(), before);
}

#[test]
fn shape_fill_and_line_keep_the_element_identity() {
    let (mut deck, slide, shape, _) = sample();
    let fill = Fill::Solid { color: "#0F172A".to_string() };
    let line = Line { color: Some("#FFFFFF".to_string()), width_emu: Some(emu(1.5)) };

    deck.transaction(None, |edit| {
        edit.set_shape_fill(&slide, &shape, Some(fill.clone()))?;
        edit.set_shape_line(&slide, &shape, Some(line.clone()))
    })
    .expect("shape style");

    let ElementContent::Shape(authored) = &deck.slide(&slide).expect("slide").element(&shape).expect("shape").content else {
        panic!("the element stays a shape");
    };
    assert_eq!(authored.fill.as_ref(), Some(&fill));
    assert_eq!(authored.line.as_ref(), Some(&line));

    deck.transaction(None, |edit| {
        edit.set_shape_fill(&slide, &shape, None)?;
        edit.set_shape_line(&slide, &shape, None)
    })
    .expect("clear shape style");
    let ElementContent::Shape(authored) = &deck.slide(&slide).expect("slide").element(&shape).expect("shape").content else {
        panic!("the element stays a shape");
    };
    assert_eq!(authored.fill, None);
    assert_eq!(authored.line, None);
}

#[test]
fn shape_style_rejects_non_shape_targets_and_rolls_back() {
    let (mut deck, slide, _, table) = sample();
    let revision = deck.revision();
    let text = deck.slide(&slide).expect("slide").elements[0].id.clone();

    let error = deck
        .transaction::<()>(None, |edit| edit.set_shape_fill(&slide, &text, Some(Fill::Solid { color: "#123456".to_string() })))
        .expect_err("text elements carry no fill");
    assert_eq!(error.code(), codes::OBJECT_TYPE_MISMATCH);
    let error = deck
        .transaction::<()>(None, |edit| edit.set_shape_line(&slide, &table, None))
        .expect_err("tables carry no stroke");
    assert_eq!(error.code(), codes::OBJECT_TYPE_MISMATCH);

    let before = deck.revision();
    let result = deck.transaction::<()>(None, |edit| {
        edit.set_shape_fill(&slide, &text, None)?;
        Err::<(), DeckError>(DeckError::new(codes::ARGUMENT_INVALID, "abort"))
    });
    assert!(result.is_err());
    assert_eq!(deck.revision(), before.max(revision), "no revision is consumed by a rejected script");
}

#[test]
fn table_widths_heights_and_cell_fills_are_written_in_place() {
    let (mut deck, slide, _, table) = sample();
    let fill = Fill::Solid { color: "#112233".to_string() };

    deck.transaction(None, |edit| {
        edit.set_table_column_widths(&slide, &table, vec![emu(50.0), emu(150.0)])?;
        edit.set_table_row_heights(&slide, &table, vec![emu(30.0), emu(40.0)])?;
        edit.set_table_cell_fill(&slide, &table, 0, 0, Some(fill.clone()))
    })
    .expect("grid edits");

    let (heights, texts, fills) = table_rows(&deck, &slide, &table);
    assert_eq!(heights, vec![emu(30.0), emu(40.0)]);
    assert_eq!(texts, vec![vec!["A".to_string(), "B".to_string()], vec!["1".to_string(), "2".to_string()]]);
    assert_eq!(fills[0][0].as_ref(), Some(&fill));
    assert_eq!(fills[1][1], None);

    let bytes = encode_project(&deck).expect("encode");
    let reopened = decode_project(&bytes).expect("decode");
    let (heights, _, fills) = table_rows(&reopened, &slide, &table);
    assert_eq!(heights, vec![emu(30.0), emu(40.0)], "row heights survive the project round trip");
    assert_eq!(fills[0][0].as_ref(), Some(&fill), "cell fills survive the project round trip");
}

#[test]
fn grid_edits_reject_count_mismatches_and_bad_values() {
    let (mut deck, slide, _, table) = sample();
    let revision = deck.revision();

    for (widths, heights) in [(vec![emu(10.0)], vec![emu(10.0)]), (vec![emu(10.0), emu(10.0)], vec![emu(10.0)])] {
        let error = deck
            .transaction::<()>(None, |edit| {
                edit.set_table_column_widths(&slide, &table, widths)?;
                edit.set_table_row_heights(&slide, &table, heights)
            })
            .expect_err("a count mismatch is rejected");
        assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    }
    let error = deck
        .transaction::<()>(None, |edit| edit.set_table_cell_fill(&slide, &table, 2, 0, None))
        .expect_err("a row outside the table is rejected");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);

    assert_eq!(deck.revision(), revision);
    let (heights, texts, _) = table_rows(&deck, &slide, &table);
    assert_eq!(heights, vec![emu(24.0), emu(24.0)]);
    assert_eq!(texts[0], vec!["A".to_string(), "B".to_string()]);
}

#[test]
fn rows_move_and_duplicate_without_reissuing_identities() {
    let (mut deck, slide, _, table) = sample();

    let index = deck.transaction(None, |edit| edit.move_table_row(&slide, &table, 1, 0)).expect("move the second row up");
    assert_eq!(index, 0);
    let (_, texts, _) = table_rows(&deck, &slide, &table);
    assert_eq!(texts, vec![vec!["1".to_string(), "2".to_string()], vec!["A".to_string(), "B".to_string()]]);

    let index = deck.transaction(None, |edit| edit.duplicate_table_row(&slide, &table, 0, None)).expect("duplicate a row");
    assert_eq!(index, 1);
    let (heights, texts, _) = table_rows(&deck, &slide, &table);
    assert_eq!(heights.len(), 3);
    assert_eq!(texts[1], texts[0], "the copy keeps the source cells");

    let element_ids: Vec<_> = deck.slide(&slide).expect("slide").elements.iter().map(|element| element.id.clone()).collect();
    assert!(element_ids.contains(&table), "the table keeps its identity");
    deck.document().validate().expect("identities stay unique");
}

#[test]
fn columns_move_and_duplicate_with_their_widths() {
    let (mut deck, slide, _, table) = sample();

    deck.transaction(None, |edit| edit.move_table_column(&slide, &table, 1, 0)).expect("move the second column left");
    let ElementContent::Table(authored) = &deck.slide(&slide).expect("slide").element(&table).expect("table").content else {
        panic!("the element stays a table");
    };
    assert_eq!(authored.column_widths_emu, vec![emu(120.0), emu(80.0)]);
    let texts: Vec<_> = authored.rows[0].cells.iter().map(|cell| cell.text.text()).collect();
    assert_eq!(texts, vec!["B".to_string(), "A".to_string()]);

    let index = deck.transaction(None, |edit| edit.duplicate_table_column(&slide, &table, 0, None)).expect("duplicate a column");
    assert_eq!(index, 1);
    let ElementContent::Table(authored) = &deck.slide(&slide).expect("slide").element(&table).expect("table").content else {
        panic!("the element stays a table");
    };
    assert_eq!(authored.column_widths_emu, vec![emu(120.0), emu(120.0), emu(80.0)]);
    let texts: Vec<_> = authored.rows[0].cells.iter().map(|cell| cell.text.text()).collect();
    assert_eq!(texts, vec!["B".to_string(), "B".to_string(), "A".to_string()]);
}

#[test]
fn row_and_column_edits_reject_out_of_range_targets_atomically() {
    let (mut deck, slide, _, table) = sample();
    let revision = deck.revision();

    let error = deck
        .transaction::<usize>(None, |edit| edit.move_table_row(&slide, &table, 2, 0))
        .expect_err("a row outside the grid is rejected");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    let error = deck
        .transaction::<usize>(None, |edit| edit.move_table_column(&slide, &table, 0, 2))
        .expect_err("a column outside the grid is rejected");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    let error = deck
        .transaction::<usize>(None, |edit| edit.duplicate_table_row(&slide, &table, 0, Some(9)))
        .expect_err("an insertion point outside the grid is rejected");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);

    assert_eq!(deck.revision(), revision);
    let (heights, texts, _) = table_rows(&deck, &slide, &table);
    assert_eq!(heights.len(), 2);
    assert_eq!(texts[0], vec!["A".to_string(), "B".to_string()]);
}
