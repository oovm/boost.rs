//! Text-run editing tests for the authored model.

use document_presentation::authored::{
    Deck, DeckCreateOptions, DeckEdit, ElementContent, ElementId, ShapeElementInput, SlideAddOptions, SlideId, TableCell, TableElementInput,
    TableRow, TextAlign, TextContent, TextElementInput, TextParagraph, TextReplaceOptions, TextRun, TextRunStyle, Transform, codes,
    decode_project, encode_project,
};

fn deck() -> Deck {
    Deck::create(DeckCreateOptions::default())
}

fn transform() -> Transform {
    Transform::from_points(40.0, 120.0, 400.0, 200.0).expect("valid box")
}

/// A deck with one slide and one two-paragraph, three-run text element.
fn deck_with_runs() -> (Deck, SlideId, ElementId) {
    let mut deck = deck();
    let slide = deck.transaction(None, |edit| edit.add_slide(SlideAddOptions::default())).expect("slide");
    let element = deck
        .transaction(None, |edit| {
            edit.add_text(
                &slide,
                TextElementInput {
                    text: TextContent {
                        paragraphs: vec![
                            TextParagraph { runs: vec![TextRun::plain("Revenue "), TextRun::plain("report")], level: 0, align: None },
                            TextParagraph { runs: vec![TextRun::plain("revenue again")], level: 0, align: None },
                        ],
                    },
                    transform: Some(transform()),
                    alt: None,
                    placeholder: None,
                },
            )
        })
        .expect("text");
    (deck, slide, element)
}

fn runs_of(deck: &Deck, slide: &SlideId, element: &ElementId) -> Vec<TextRun> {
    let target = deck.slide(slide).expect("slide").element(element).expect("element");
    let ElementContent::Text(text) = &target.content
    else {
        panic!("expected a text element");
    };
    text.paragraphs.iter().flat_map(|paragraph| paragraph.runs.clone()).collect()
}

#[test]
fn style_text_runs_stamps_every_run_and_keeps_omitted_fields() {
    let (mut deck, slide, element) = deck_with_runs();
    let style = TextRunStyle { bold: Some(true), color: Some("#00AA00".to_string()), ..TextRunStyle::default() };
    deck.transaction(None, |edit: &mut DeckEdit| edit.style_text_runs(&slide, &element, &style)).expect("style");

    let runs = runs_of(&deck, &slide, &element);
    assert_eq!(runs.len(), 3);
    assert!(runs.iter().all(|run| run.bold == Some(true)));
    assert!(runs.iter().all(|run| run.color.as_deref() == Some("#00AA00")));
    // Untouched properties keep the run's own value.
    assert!(runs.iter().all(|run| run.italic.is_none() && run.size_pt.is_none()));
    // The identity survives a styled rewrite.
    assert_eq!(deck.slide(&slide).expect("slide").element(&element).expect("element").id, element);

    let align = TextRunStyle { align: Some(TextAlign::Center), ..TextRunStyle::default() };
    deck.transaction(None, |edit| edit.style_text_runs(&slide, &element, &align)).expect("align");
    let target = deck.slide(&slide).expect("slide").element(&element).expect("element");
    let ElementContent::Text(text) = &target.content
    else {
        panic!("expected a text element");
    };
    assert!(text.paragraphs.iter().all(|paragraph| paragraph.align == Some(TextAlign::Center)));
}

#[test]
fn text_styles_reject_empty_invalid_and_wrong_kinds() {
    let (mut deck, slide, element) = deck_with_runs();
    let shape = deck
        .transaction(None, |edit| {
            edit.add_shape(
                &slide,
                ShapeElementInput { geometry: "rect".to_string(), transform: Some(transform()), fill: None, line: None, alt: None },
            )
        })
        .expect("shape");
    let revision = deck.revision();

    let cases: Vec<(TextRunStyle, &str)> = vec![
        (TextRunStyle::default(), codes::ARGUMENT_INVALID),
        (TextRunStyle { size_pt: Some(0.0), ..TextRunStyle::default() }, codes::ARGUMENT_INVALID),
        (TextRunStyle { color: Some("  ".to_string()), ..TextRunStyle::default() }, codes::ARGUMENT_INVALID),
        (TextRunStyle { font_family: Some("  ".to_string()), ..TextRunStyle::default() }, codes::ARGUMENT_INVALID),
    ];
    for (style, code) in cases {
        let error = deck.transaction::<()>(None, |edit| edit.style_text_runs(&slide, &element, &style)).expect_err("unusable style");
        assert_eq!(error.code(), code);
    }
    let style = TextRunStyle { bold: Some(true), ..TextRunStyle::default() };
    let error = deck.transaction::<()>(None, |edit| edit.style_text_runs(&slide, &shape, &style)).expect_err("a shape has no runs");
    assert_eq!(error.code(), codes::OBJECT_TYPE_MISMATCH);
    let unknown = ElementId::new("element-absent").expect("id");
    let error = deck.transaction::<()>(None, |edit| edit.style_text_runs(&slide, &unknown, &style)).expect_err("unknown element");
    assert_eq!(error.code(), codes::OBJECT_NOT_FOUND);

    assert_eq!(deck.revision(), revision, "failed styles consume no revision");
    assert!(runs_of(&deck, &slide, &element).iter().all(|run| run.bold.is_none()));
}

#[test]
fn hyperlink_stamps_and_clears_every_run() {
    let (mut deck, slide, element) = deck_with_runs();
    deck.transaction(None, |edit| edit.set_text_hyperlink(&slide, &element, Some("https://example.com/report".to_string()))).expect("link");
    assert!(runs_of(&deck, &slide, &element).iter().all(|run| run.hyperlink.as_deref() == Some("https://example.com/report")));

    deck.transaction(None, |edit| edit.set_text_hyperlink(&slide, &element, None)).expect("clear");
    assert!(runs_of(&deck, &slide, &element).iter().all(|run| run.hyperlink.is_none()));

    let revision = deck.revision();
    let error =
        deck.transaction::<()>(None, |edit| edit.set_text_hyperlink(&slide, &element, Some("   ".to_string()))).expect_err("blank link");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    let shape = deck
        .transaction(None, |edit| {
            edit.add_shape(
                &slide,
                ShapeElementInput { geometry: "rect".to_string(), transform: Some(transform()), fill: None, line: None, alt: None },
            )
        })
        .expect("shape");
    let error = deck
        .transaction::<()>(None, |edit| edit.set_text_hyperlink(&slide, &shape, Some("https://x.test".to_string())))
        .expect_err("a shape has no runs");
    assert_eq!(error.code(), codes::OBJECT_TYPE_MISMATCH);
    assert_eq!(deck.revision(), revision + 1, "only the shape commit consumed a revision");
}

#[test]
fn style_table_cell_targets_only_the_addressed_cell() {
    let mut deck = deck();
    let (slide, table) = deck
        .transaction(None, |edit| {
            let slide = edit.add_slide(SlideAddOptions::default())?;
            let rows =
                (0..2).map(|_| TableRow { height_emu: 200_000, cells: vec![TableCell::plain("revenue"), TableCell::plain("cost")] }).collect();
            let table = edit.add_table(
                &slide,
                TableElementInput { column_widths_emu: vec![400_000, 400_000], rows, transform: Some(transform()), alt: None },
            )?;
            Ok((slide, table))
        })
        .expect("table");

    let style = TextRunStyle { bold: Some(true), size_pt: Some(28.0), ..TextRunStyle::default() };
    deck.transaction(None, |edit| edit.style_table_cell(&slide, &table, 1, 0, &style)).expect("cell style");

    let target = deck.slide(&slide).expect("slide").element(&table).expect("element");
    let ElementContent::Table(content) = &target.content
    else {
        panic!("expected a table");
    };
    assert_eq!(content.rows[1].cells[0].text.paragraphs[0].runs[0].bold, Some(true));
    assert_eq!(content.rows[1].cells[0].text.paragraphs[0].runs[0].size_pt, Some(28.0));
    assert!(
        content.rows[0].cells.iter().chain(content.rows[1].cells.iter().skip(1)).all(|cell| cell.text.paragraphs[0].runs[0].bold.is_none())
    );

    let text = deck
        .transaction(None, |edit| {
            edit.add_text(
                &slide,
                TextElementInput { text: TextContent::plain("字"), transform: Some(transform()), alt: None, placeholder: None },
            )
        })
        .expect("text");
    let revision = deck.revision();
    let error = deck.transaction::<()>(None, |edit| edit.style_table_cell(&slide, &table, 5, 0, &style)).expect_err("row outside");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    let error = deck.transaction::<()>(None, |edit| edit.style_table_cell(&slide, &table, 0, 5, &style)).expect_err("column outside");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    let error = deck.transaction::<()>(None, |edit| edit.style_table_cell(&slide, &text, 0, 0, &style)).expect_err("not a table");
    assert_eq!(error.code(), codes::OBJECT_TYPE_MISMATCH);
    assert_eq!(deck.revision(), revision, "failed cell styles consume no revision");
}

#[test]
fn replace_text_rewrites_runs_and_cells_deck_wide() {
    let (mut deck, slide, element) = deck_with_runs();
    let other = deck.transaction(None, |edit| edit.add_slide(SlideAddOptions::default())).expect("second slide");
    let table = deck
        .transaction(None, |edit| {
            edit.add_table(
                &other,
                TableElementInput {
                    column_widths_emu: vec![400_000],
                    rows: vec![TableRow { height_emu: 200_000, cells: vec![TableCell::plain("Revenue: 12")] }],
                    transform: Some(transform()),
                    alt: None,
                },
            )
        })
        .expect("table");

    let hits = deck.transaction(None, |edit| edit.replace_text("revenue", "income", TextReplaceOptions::default())).expect("replace");
    assert_eq!(hits, 3, "two runs on the first slide plus the cell");
    let texts: Vec<String> = runs_of(&deck, &slide, &element).into_iter().map(|run| run.text).collect();
    assert_eq!(texts, vec!["income ", "report", "income again"]);
    let target = deck.slide(&other).expect("slide").element(&table).expect("element");
    let ElementContent::Table(content) = &target.content
    else {
        panic!("expected a table");
    };
    assert_eq!(content.rows[0].cells[0].text.text(), "income: 12");

    // Case-sensitive matching only touches the exact form.
    let hits = deck
        .transaction(None, |edit| {
            edit.replace_text("Income", "profit", TextReplaceOptions { case_sensitive: true, ..TextReplaceOptions::default() })
        })
        .expect("case-sensitive replace");
    assert_eq!(hits, 0, "the stored forms are lowercase");

    // Whole-word matching refuses matches that touch a word character.
    let hits = deck
        .transaction(None, |edit| {
            edit.replace_text("income", "revenue", TextReplaceOptions { whole_word: true, ..TextReplaceOptions::default() })
        })
        .expect("whole-word replace");
    assert_eq!(hits, 3, "every stored `income` is followed by a space or a colon");
    let texts: Vec<String> = runs_of(&deck, &slide, &element).into_iter().map(|run| run.text).collect();
    assert_eq!(texts, vec!["revenue ", "report", "revenue again"]);

    // The slide filter restricts the rewrite; unknown identities are ignored.
    let hits = deck
        .transaction(None, |edit| {
            edit.replace_text(
                "revenue",
                "earnings",
                TextReplaceOptions {
                    slides: Some(vec![SlideId::new("slide-absent").expect("id"), other.clone()]),
                    ..TextReplaceOptions::default()
                },
            )
        })
        .expect("filtered replace");
    assert_eq!(hits, 1, "only the table on the second slide matches");
    assert_eq!(runs_of(&deck, &slide, &element)[0].text, "revenue ");
}

#[test]
fn replace_text_leaves_matches_spanning_runs_untouched() {
    let (mut deck, _slide, element) = deck_with_runs();
    let hits =
        deck.transaction(None, |edit| edit.replace_text("revenue report", "summary", TextReplaceOptions::default())).expect("spanning replace");
    assert_eq!(hits, 0);
    assert_eq!(runs_of(&deck, &_slide, &element)[0].text, "Revenue ");
}

#[test]
fn replace_text_rejects_blank_find() {
    let (mut deck, _slide, _element) = deck_with_runs();
    let revision = deck.revision();
    let error = deck.transaction::<usize>(None, |edit| edit.replace_text("", "x", TextReplaceOptions::default())).expect_err("blank find");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    assert_eq!(deck.revision(), revision);
}

#[test]
fn failed_text_run_transaction_rolls_back_completely() {
    let (mut deck, slide, element) = deck_with_runs();
    let revision = deck.revision();
    let style = TextRunStyle { bold: Some(true), ..TextRunStyle::default() };
    let unknown = ElementId::new("element-absent").expect("id");
    let result = deck.transaction::<()>(None, |edit| {
        edit.style_text_runs(&slide, &element, &style)?;
        edit.set_text_hyperlink(&slide, &unknown, Some("https://x.test".to_string()))
    });
    assert_eq!(result.expect_err("unknown element").code(), codes::OBJECT_NOT_FOUND);
    assert_eq!(deck.revision(), revision, "the aborted script consumed no revision");
    assert!(runs_of(&deck, &slide, &element).iter().all(|run| run.bold.is_none()), "the style rolled back with the draft");
}

#[test]
fn text_run_edits_survive_project_roundtrip() {
    let (mut deck, slide, element) = deck_with_runs();
    let style = TextRunStyle { bold: Some(true), color: Some("#00AA00".to_string()), size_pt: Some(28.0), ..TextRunStyle::default() };
    deck.transaction(None, |edit| {
        edit.style_text_runs(&slide, &element, &style)?;
        edit.set_text_hyperlink(&slide, &element, Some("https://example.com".to_string()))?;
        edit.replace_text("Revenue", "Income", TextReplaceOptions::default())
    })
    .expect("edits");

    let bytes = encode_project(&deck).expect("encode");
    let reopened = decode_project(&bytes).expect("decode");
    assert_eq!(reopened.revision(), deck.revision());
    let runs = runs_of(&reopened, &slide, &element);
    assert_eq!(runs[0].text, "Income ");
    assert!(runs.iter().all(|run| run.bold == Some(true)));
    assert!(runs.iter().all(|run| run.color.as_deref() == Some("#00AA00")));
    assert!(runs.iter().all(|run| run.hyperlink.as_deref() == Some("https://example.com")));
    assert_eq!(encode_project(&reopened).expect("re-encode"), bytes);
}
