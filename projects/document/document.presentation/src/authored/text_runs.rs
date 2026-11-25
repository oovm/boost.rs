//! Typed text-run editing operations on a [`DeckEdit`] draft.
//!
//! These methods carry the run-level editing semantics the OpenPPT product
//! exposed through JSON script operations (`element.setTextStyle`,
//! `element.setHyperlink`, `table.setCellStyle`, `deck.replaceText`), but with
//! typed Rust inputs and outputs. Replacing a whole text body stays on
//! [`DeckEdit::set_element_text`](super::edit::DeckEdit::set_element_text) and
//! plain cell text stays on
//! [`DeckEdit::set_table_cell_text`](super::tables::DeckEdit::set_table_cell_text).
//!
//! Every operation mutates the draft in place; atomicity and rollback come from
//! [`Deck::transaction`](super::model::Deck::transaction). Object identities are
//! preserved: only run fields and run texts change, never element ids.
//! Replacement matches per run, so a match spanning two runs stays untouched.

use super::{
    edit::DeckEdit,
    error::{DeckError, codes, params::ObjectKind},
    id::{ElementId, SlideId},
    model::{ElementContent, TableContent, TableRow, TextAlign, TextContent},
};

/// Run-level styling shared by the text style operations.
///
/// Every set field is written onto every run it addresses; omitted or `None`
/// fields keep the run's current value. `align` is paragraph-level and is
/// written onto every paragraph.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextRunStyle {
    /// Bold flag.
    pub bold: Option<bool>,
    /// Italic flag.
    pub italic: Option<bool>,
    /// Underline flag.
    pub underline: Option<bool>,
    /// Strikethrough flag.
    pub strike: Option<bool>,
    /// Font size in points, `1.0..=4000.0`.
    pub size_pt: Option<f64>,
    /// Text colour, e.g. `#RRGGBB`.
    pub color: Option<String>,
    /// Font family name.
    pub font_family: Option<String>,
    /// Paragraph alignment.
    pub align: Option<TextAlign>,
}

impl TextRunStyle {
    /// Whether the style carries no property at all.
    pub fn is_empty(&self) -> bool {
        self.bold.is_none()
            && self.italic.is_none()
            && self.underline.is_none()
            && self.strike.is_none()
            && self.size_pt.is_none()
            && self.color.is_none()
            && self.font_family.is_none()
            && self.align.is_none()
    }

    /// Stamps every set property onto every paragraph and run of `text`.
    pub fn apply(&self, text: &mut TextContent) {
        for paragraph in &mut text.paragraphs {
            if let Some(value) = self.align {
                paragraph.align = Some(value);
            }
            for run in &mut paragraph.runs {
                if let Some(value) = self.bold {
                    run.bold = Some(value);
                }
                if let Some(value) = self.italic {
                    run.italic = Some(value);
                }
                if let Some(value) = self.underline {
                    run.underline = Some(value);
                }
                if let Some(value) = self.strike {
                    run.strike = Some(value);
                }
                if let Some(value) = self.size_pt {
                    run.size_pt = Some(value);
                }
                if let Some(value) = &self.color {
                    run.color = Some(value.clone());
                }
                if let Some(value) = &self.font_family {
                    run.font_family = Some(value.clone());
                }
            }
        }
    }

    /// Rejects a style with no property or with unusable values.
    fn validate(&self) -> Result<(), DeckError> {
        if self.is_empty() {
            return Err(invalid("a text style needs at least one property"));
        }
        if let Some(size) = self.size_pt
            && !(size.is_finite() && (1.0..=4000.0).contains(&size))
        {
            return Err(invalid("`size_pt` must be between 1 and 4000 points"));
        }
        if self.color.as_ref().is_some_and(|color| color.trim().is_empty()) {
            return Err(invalid("`color` must not be blank"));
        }
        if self.font_family.as_ref().is_some_and(|family| family.trim().is_empty()) {
            return Err(invalid("`font_family` must not be blank"));
        }
        Ok(())
    }
}

/// Options accepted by [`DeckEdit::replace_text`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextReplaceOptions {
    /// Compare characters exactly instead of case-folded.
    pub case_sensitive: bool,
    /// Refuse matches that touch a letter, digit or underscore.
    pub whole_word: bool,
    /// Restrict the rewrite to these slides; unknown identities are ignored.
    pub slides: Option<Vec<SlideId>>,
}

impl DeckEdit {
    /// Stamps a run style onto every run of one text element.
    pub fn style_text_runs(&mut self, slide: &SlideId, element: &ElementId, style: &TextRunStyle) -> Result<(), DeckError> {
        style.validate()?;
        let text = self.text_mut(slide, element, "only text elements carry text runs")?;
        style.apply(text);
        Ok(())
    }

    /// Sets or clears the hyperlink carried by every run of one text element.
    ///
    /// A provided destination must not be blank; `None` clears the links.
    pub fn set_text_hyperlink(&mut self, slide: &SlideId, element: &ElementId, url: Option<String>) -> Result<(), DeckError> {
        if let Some(url) = &url
            && url.trim().is_empty()
        {
            return Err(invalid("a hyperlink destination must not be blank"));
        }
        let text = self.text_mut(slide, element, "only text elements carry text runs")?;
        for paragraph in &mut text.paragraphs {
            for run in &mut paragraph.runs {
                run.hyperlink = url.clone();
            }
        }
        Ok(())
    }

    /// Stamps a run style onto every run of one table cell.
    pub fn style_table_cell(
        &mut self,
        slide: &SlideId,
        element: &ElementId,
        row: usize,
        column: usize,
        style: &TextRunStyle,
    ) -> Result<(), DeckError> {
        style.validate()?;
        let table = self.table_mut(slide, element, "only tables carry cells")?;
        assert_cell_bounds(table, row, column)?;
        style.apply(&mut table.rows[row].cells[column].text);
        Ok(())
    }

    /// Replaces every match of `find` in the deck's text elements and table cells.
    ///
    /// Matching happens per run, so a match spanning two runs stays untouched.
    /// Returns the number of replaced matches.
    pub fn replace_text(
        &mut self,
        find: impl Into<String>,
        replace: impl Into<String>,
        options: TextReplaceOptions,
    ) -> Result<usize, DeckError> {
        let find = find.into();
        if find.is_empty() {
            return Err(invalid("`find` must be a non-empty string"));
        }
        let replace = replace.into();
        let only = options.slides.map(|ids| ids.into_iter().collect::<std::collections::BTreeSet<_>>());
        let mut replacements = 0_usize;
        for model in &mut self.document_mut().slides {
            if only.as_ref().is_some_and(|ids| !ids.contains(&model.id)) {
                continue;
            }
            for element in &mut model.elements {
                match &mut element.content {
                    ElementContent::Text(text) => {
                        replacements += replace_in_text(text, &find, &replace, options.case_sensitive, options.whole_word)
                    }
                    ElementContent::Table(table) => {
                        for row in &mut table.rows {
                            for cell in &mut row.cells {
                                replacements += replace_in_text(&mut cell.text, &find, &replace, options.case_sensitive, options.whole_word);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(replacements)
    }

    /// Borrows the rich text of one element, rejecting non-text targets.
    fn text_mut(&mut self, slide: &SlideId, element: &ElementId, needs: &str) -> Result<&mut TextContent, DeckError> {
        let model = self.slide_mut(slide)?;
        let target = model.element_mut(element).ok_or_else(|| missing_element(slide, element))?;
        match &mut target.content {
            ElementContent::Text(text) => Ok(text),
            _ => Err(DeckError::new(codes::OBJECT_TYPE_MISMATCH, format!("element `{element}` is not a text element; {needs}"))
                .with_slide(slide.clone())
                .with_element(element.clone())),
        }
    }
}

/// Rebuilds one rich-text body in place, replacing every match inside its runs.
/// Returns the number of replaced matches.
pub fn replace_in_text(text: &mut TextContent, find: &str, replace: &str, case_sensitive: bool, whole_word: bool) -> usize {
    let mut hits = 0_usize;
    for paragraph in &mut text.paragraphs {
        for run in &mut paragraph.runs {
            let (next, count) = replace_matches(&run.text, find, replace, case_sensitive, whole_word);
            if count > 0 {
                run.text = next;
                hits += count;
            }
        }
    }
    hits
}

/// Rebuilds table rows with every match of `find` replaced inside the cell texts.
pub fn replace_in_rows(rows: &[TableRow], find: &str, replace: &str, case_sensitive: bool, whole_word: bool) -> (Vec<TableRow>, usize) {
    let mut rows = rows.to_vec();
    let mut hits = 0_usize;
    for row in &mut rows {
        for cell in &mut row.cells {
            hits += replace_in_text(&mut cell.text, find, replace, case_sensitive, whole_word);
        }
    }
    (rows, hits)
}

/// Replaces non-overlapping matches of `find` in one run of text.
///
/// Case folding compares characters, and `whole_word` refuses matches that
/// touch a letter, digit or underscore.
fn replace_matches(text: &str, find: &str, replace: &str, case_sensitive: bool, whole_word: bool) -> (String, usize) {
    let needle = find.chars().collect::<Vec<_>>();
    if text.is_empty() || needle.is_empty() {
        return (text.to_string(), 0);
    }
    let haystack = text.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0_usize;
    let mut hits = 0_usize;
    while cursor < haystack.len() {
        let matched =
            matches_at(&haystack, cursor, &needle, case_sensitive) && !(whole_word && touches_word_char(&haystack, cursor, needle.len()));
        if matched {
            output.push_str(replace);
            cursor += needle.len();
            hits += 1;
        }
        else {
            output.push(haystack[cursor]);
            cursor += 1;
        }
    }
    (output, hits)
}

/// Whether `needle` starts at `start` in `haystack` under the case option.
fn matches_at(haystack: &[char], start: usize, needle: &[char], case_sensitive: bool) -> bool {
    haystack.len() >= start + needle.len()
        && haystack[start..]
            .iter()
            .zip(needle)
            .all(|(found, wanted)| found == wanted || (!case_sensitive && found.to_lowercase().eq(wanted.to_lowercase())))
}

/// Whether the characters around a match are letters, digits or underscores.
fn touches_word_char(text: &[char], start: usize, length: usize) -> bool {
    let before = if start == 0 { None } else { Some(text[start - 1]) };
    let after = text.get(start + length).copied();
    is_word_char(before) || is_word_char(after)
}

fn is_word_char(value: Option<char>) -> bool {
    value.is_some_and(|character| character.is_alphanumeric() || character == '_')
}

fn assert_cell_bounds(table: &TableContent, row: usize, column: usize) -> Result<(), DeckError> {
    if row >= table.rows.len() {
        return Err(invalid(format!("`row` {row} is outside the table's {} rows", table.rows.len())));
    }
    let columns = table.rows[row].cells.len();
    if column >= columns {
        return Err(invalid(format!("`column` {column} is outside row {row}'s {columns} cells")));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> DeckError {
    DeckError::new(codes::ARGUMENT_INVALID, message)
}

fn missing_element(slide: &SlideId, element: &ElementId) -> DeckError {
    DeckError::unknown(ObjectKind::Element, element.as_str())
        .with_slide(slide.clone())
        .with_element(element.clone())
}
