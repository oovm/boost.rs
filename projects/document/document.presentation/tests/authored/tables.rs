//! Tests for the typed table operations in `authored::tables`.
//!
//! They cover the acceptance contract of this batch: a commit takes effect
//! atomically, a failure leaves no half-written table, edits keep the stable
//! element identity, duplication allocates a fresh one, and transposition maps
//! every cell to its mirrored position.

use document_presentation::authored::{
    Deck, DeckCreateOptions, ElementId, ElementKind, SlideAddOptions, SlideId, TableColumnInsertInput, TableContent, TableCell,
    TableDuplicateOptions, TableElementInput, TableMoveTarget, TableRow, TableRowInsertInput, TextContent, TextElementInput, codes,
};

fn table_input() -> TableElementInput {
    TableElementInput {
        column_widths_emu: vec![100_000, 200_000, 300_000],
        rows: vec![
            TableRow { height_emu: 50_000, cells: vec![TableCell::plain("a1"), TableCell::plain("b1"), TableCell::plain("c1")] },
            TableRow { height_emu: 60_000, cells: vec![TableCell::plain("a2"), TableCell::plain("b2"), TableCell::plain("c2")] },
        ],
        transform: None,
        alt: Some("data table".to_string()),
    }
}

fn deck_with_table() -> (Deck, SlideId, ElementId) {
    let mut deck = Deck::create(DeckCreateOptions::default());
    let (slide, element) = deck
        .transaction(None, |edit| {
            let slide = edit.add_slide(SlideAddOptions::default())?;
            let element = edit.add_table(&slide, table_input())?;
            Ok((slide, element))
        })
        .expect("commit table");
    (deck, slide, element)
}

fn table_of(deck: &Deck, slide: &SlideId, element: &ElementId) -> TableContent {
    let ElementKind::Table = deck.slide(slide).expect("slide").element(element).expect("element").kind else {
        panic!("element is not a table");
    };
    match &deck.slide(slide).expect("slide").element(element).expect("element").content {
        document_presentation::authored::ElementContent::Table(table) => table.clone(),
        _ => panic!("element is not a table"),
    }
}

fn cell_text(table: &TableContent, row: usize, column: usize) -> String {
    table.rows[row].cells[column].text.text()
}

#[test]
fn add_table_rejects_malformed_input_without_touching_the_deck() {
    let mut deck = Deck::create(DeckCreateOptions::default());
    let result = deck.transaction(None, |edit| {
        let slide = edit.add_slide(SlideAddOptions::default())?;
        let mut broken = table_input();
        broken.rows[1].cells.pop();
        edit.add_table(&slide, broken)?;
        Ok(())
    });
    let error = result.expect_err("ragged rows are rejected");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    assert_eq!(deck.revision(), 0);
    assert!(deck.slides().is_empty(), "the drafted slide must not survive the failed commit");
    assert_eq!(deck.undo_depth(), 0);
}

#[test]
fn remove_table_deletes_only_tables() {
    let (mut deck, slide, element) = deck_with_table();
    let text = deck
        .transaction(None, |edit| {
            edit.add_text(
                &slide,
                TextElementInput { text: TextContent::plain("not a table"), transform: None, alt: None, placeholder: None },
            )
        })
        .expect("commit text");
    let error = deck
        .transaction(None, |edit| edit.remove_table(&slide, &text))
        .expect_err("a text element is not a table");
    assert_eq!(error.code(), codes::OBJECT_TYPE_MISMATCH);
    assert_eq!(deck.slides()[0].elements.len(), 2, "the rejected removal changed nothing");

    deck.transaction(None, |edit| edit.remove_table(&slide, &element)).expect("remove table");
    assert!(deck.slides()[0].element(&element).is_none());
    assert_eq!(deck.slides()[0].elements.len(), 1);
}

#[test]
fn table_edits_keep_the_element_identity() {
    let (mut deck, slide, element) = deck_with_table();
    deck.transaction(None, |edit| {
        edit.set_table_cell_text(&slide, &element, 0, 1, "b1*")?;
        edit.insert_table_row(
            &slide,
            &element,
            TableRowInsertInput { index: 1, cells: Some(vec!["x1".to_string(), "x2".to_string(), "x3".to_string()]), height_emu: Some(55_000) },
        )?;
        edit.insert_table_column(
            &slide,
            &element,
            TableColumnInsertInput { index: 1, cells: Some(vec!["y1".to_string(), "y2".to_string(), "y3".to_string()]), width_emu: Some(150_000) },
        )?;
        edit.delete_table_row(&slide, &element, 2)?;
        edit.delete_table_column(&slide, &element, 2)?;
        Ok(())
    })
    .expect("commit table edits");

    let target = deck.slide(&slide).expect("slide").element(&element).expect("identity survives edits");
    assert_eq!(target.id, element);
    assert_eq!(target.kind, ElementKind::Table);
    let table = table_of(&deck, &slide, &element);
    assert_eq!(table.rows.len(), 2);
    assert_eq!(table.column_widths_emu.len(), 3);
    assert_eq!(cell_text(&table, 0, 1), "y1");
    assert_eq!(cell_text(&table, 1, 1), "y2");
    deck.document().validate().expect("identities stay unique");
}

#[test]
fn failed_table_edit_rolls_back_without_half_state() {
    let (mut deck, slide, element) = deck_with_table();
    let revision = deck.revision();
    let result = deck.transaction(None, |edit| {
        edit.set_table_cell_text(&slide, &element, 0, 0, "changed")?;
        edit.delete_table_row(&slide, &element, 99)?;
        Ok(())
    });
    let error = result.expect_err("an out-of-range row is rejected");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    assert_eq!(deck.revision(), revision, "a failed transaction consumes no revision");
    assert_eq!(cell_text(&table_of(&deck, &slide, &element), 0, 0), "a1", "the earlier cell write must not survive");
    assert_eq!(table_of(&deck, &slide, &element).rows.len(), 2);

    deck.transaction(None, |edit| edit.set_table_cell_text(&slide, &element, 1, 2, "still editable"))
        .expect("the deck stays usable after a rollback");
}

#[test]
fn merged_cells_block_structural_edits() {
    let mut deck = Deck::create(DeckCreateOptions::default());
    let (slide, vertical, horizontal) = deck
        .transaction(None, |edit| {
            let slide = edit.add_slide(SlideAddOptions::default())?;
            let mut rows = table_input().rows;
            rows[0].cells[0].row_span = 2;
            let vertical = edit.add_table(
                &slide,
                TableElementInput { column_widths_emu: vec![100_000, 200_000, 300_000], rows, transform: None, alt: None },
            )?;
            let mut rows = vec![
                TableRow { height_emu: 50_000, cells: vec![TableCell::plain("wide"), TableCell::plain("c")] },
                TableRow { height_emu: 50_000, cells: vec![TableCell::plain("a"), TableCell::plain("d")] },
            ];
            rows[0].cells[0].column_span = 2;
            let horizontal = edit.add_table(
                &slide,
                TableElementInput { column_widths_emu: vec![100_000, 200_000], rows, transform: None, alt: None },
            )?;
            Ok((slide, vertical, horizontal))
        })
        .expect("commit merged tables");

    let error = deck
        .transaction(None, |edit| {
            edit.insert_table_row(&slide, &vertical, TableRowInsertInput { index: 1, cells: None, height_emu: None })
        })
        .expect_err("a vertical merge spans the insertion point");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);

    let error = deck
        .transaction(None, |edit| edit.delete_table_row(&slide, &vertical, 1))
        .expect_err("the deleted row is covered by a vertical merge");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);

    let error = deck
        .transaction(None, |edit| edit.delete_table_column(&slide, &horizontal, 1))
        .expect_err("the deleted column is covered by a horizontal merge");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);

    let error = deck
        .transaction(None, |edit| edit.transpose_table(&slide, &horizontal))
        .expect_err("transposition needs a table without merged cells");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    assert_eq!(deck.revision(), 1, "every rejected edit consumed no revision");
    assert_eq!(table_of(&deck, &slide, &vertical).rows.len(), 2);
    assert_eq!(table_of(&deck, &slide, &horizontal).rows[0].cells.len(), 2);
}

#[test]
fn transpose_swaps_cells_widths_and_heights() {
    let (mut deck, slide, element) = deck_with_table();
    let before = table_of(&deck, &slide, &element);
    deck.transaction(None, |edit| edit.transpose_table(&slide, &element)).expect("transpose");
    let after = table_of(&deck, &slide, &element);

    assert_eq!(after.rows.len(), before.column_widths_emu.len());
    assert_eq!(after.column_widths_emu, before.rows.iter().map(|row| row.height_emu).collect::<Vec<_>>());
    for (column, width) in before.column_widths_emu.iter().enumerate() {
        assert_eq!(after.rows[column].height_emu, *width);
        for (row, source) in before.rows.iter().enumerate() {
            assert_eq!(cell_text(&after, column, row), source.cells[column].text.text(), "cell ({row}, {column}) must land at ({column}, {row})");
        }
    }
    assert_eq!(cell_text(&after, 0, 1), "a2");
    assert_eq!(cell_text(&after, 2, 0), "c1");
    assert_eq!(deck.slide(&slide).expect("slide").element(&element).expect("identity survives transposition").id, element);
}

#[test]
fn move_table_across_slides_and_within_a_slide_keeps_identity() {
    let (mut deck, first, element) = deck_with_table();
    let second = deck.transaction(None, |edit| edit.add_slide(SlideAddOptions::default())).expect("second slide");

    let index = deck
        .transaction(None, |edit| edit.move_table(&first, &element, TableMoveTarget { slide: second.clone(), index: Some(0) }))
        .expect("move across slides");
    assert_eq!(index, 0);
    assert!(deck.slide(&first).expect("source slide").element(&element).is_none());
    assert_eq!(deck.slide(&second).expect("target slide").element(&element).expect("identity survives the move").id, element);

    let other = deck.transaction(None, |edit| edit.add_table(&second, table_input())).expect("second table");
    deck.transaction(None, |edit| edit.move_table(&second, &element, TableMoveTarget { slide: second.clone(), index: Some(1) }))
        .expect("reorder within the slide");
    let order: Vec<_> = deck.slide(&second).expect("slide").elements.iter().map(|candidate| candidate.id.clone()).collect();
    assert_eq!(order, vec![other.clone(), element.clone()]);
    deck.document().validate().expect("identities stay unique");
}

#[test]
fn duplicate_table_creates_a_new_identity() {
    let (mut deck, slide, element) = deck_with_table();
    let second = deck.transaction(None, |edit| edit.add_slide(SlideAddOptions::default())).expect("second slide");
    let (copy, on_second) = deck
        .transaction(None, |edit| {
            let copy = edit.duplicate_table(
                &slide,
                &element,
                TableDuplicateOptions { target_slide: None, offset_x_emu: Some(10_000), offset_y_emu: Some(20_000) },
            )?;
            let on_second = edit.duplicate_table(&slide, &element, TableDuplicateOptions { target_slide: Some(second.clone()), ..Default::default() })?;
            Ok((copy, on_second))
        })
        .expect("duplicate tables");

    assert_ne!(copy, element);
    assert_ne!(on_second, element);
    assert_ne!(copy, on_second);
    let source = deck.slide(&slide).expect("slide").element(&element).expect("source survives");
    let duplicated = deck.slide(&slide).expect("slide").element(&copy).expect("copy on the source slide");
    assert_eq!(duplicated.content, source.content);
    assert_eq!(duplicated.alt.as_deref(), Some("data table"));
    assert_eq!(duplicated.transform.x_emu, source.transform.x_emu + 10_000);
    assert_eq!(duplicated.transform.y_emu, source.transform.y_emu + 20_000);
    assert!(deck.slide(&second).expect("second slide").element(&on_second).is_some());
    deck.document().validate().expect("identities stay unique");
}

#[test]
fn insert_row_and_column_inherit_neighbour_defaults() {
    let (mut deck, slide, element) = deck_with_table();
    deck.transaction(None, |edit| {
        edit.insert_table_row(&slide, &element, TableRowInsertInput { index: 1, cells: None, height_emu: None })?;
        edit.insert_table_column(&slide, &element, TableColumnInsertInput { index: 3, cells: None, width_emu: None })?;
        Ok(())
    })
    .expect("commit default inserts");
    let table = table_of(&deck, &slide, &element);

    assert_eq!(table.rows.len(), 3);
    assert_eq!(table.rows[1].height_emu, 60_000, "the row at the insertion point donates its height");
    assert!(table.rows[1].cells.iter().all(|cell| cell.text.text().is_empty()));
    assert_eq!(table.column_widths_emu, vec![100_000, 200_000, 300_000, 300_000], "appending inherits the last width");
    assert!(table.rows.iter().all(|row| row.cells.len() == 4));
    assert_eq!(cell_text(&table, 0, 0), "a1");
}
