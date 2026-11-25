//! Committed edits, transactions and undo.
//!
//! Every edit runs against a draft document. A draft is committed only when the
//! whole closure succeeds, so a failure never leaves a half-written slide, and a
//! revision is consumed only by a real commit.

use super::{
    error::{DeckError, codes, params::ObjectKind},
    id::{AssetId, ElementId, LayoutId, SlideId},
    size::DeckSize,
    model::{
        Asset, ChartContent, ChartData, ChartType, Deck, DeckDocument, DeckSlide, Element, ElementContent, ElementKind, ElementOrigin,
        ElementStyle, Fill, FormulaContent, ImageContent, Line, LockFlags, MediaContent, MediaKind, OriginKind, PlaceholderRef, ShapeContent,
        TableContent, TableRow, TextContent, Transform,
    },
};
use sha2::{Digest, Sha256};

/// Options accepted when a slide is added.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlideAddOptions {
    /// Layout applied to the new slide.
    pub layout: Option<LayoutId>,
    /// Insertion position; appends when absent.
    pub index: Option<usize>,
}

/// One text element to add.
#[derive(Debug, Clone, PartialEq)]
pub struct TextElementInput {
    /// Rich text content.
    pub text: TextContent,
    /// Placement; a default box is used when absent.
    pub transform: Option<Transform>,
    /// Alternative text.
    pub alt: Option<String>,
    /// Layout placeholder binding.
    pub placeholder: Option<PlaceholderRef>,
}

/// One image element to add.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageElementInput {
    /// Referenced asset.
    pub asset: AssetId,
    /// Placement; a default box is used when absent.
    pub transform: Option<Transform>,
    /// Required alternative text.
    pub alt: String,
}

/// One preset shape to add.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapeElementInput {
    /// Preset geometry name.
    pub geometry: String,
    /// Placement; a default box is used when absent.
    pub transform: Option<Transform>,
    /// Fill override.
    pub fill: Option<Fill>,
    /// Line override.
    pub line: Option<Line>,
    /// Alternative text.
    pub alt: Option<String>,
}

/// One table to add.
#[derive(Debug, Clone, PartialEq)]
pub struct TableElementInput {
    /// Column widths in EMU.
    pub column_widths_emu: Vec<i64>,
    /// Rows sharing one cell count.
    pub rows: Vec<TableRow>,
    /// Placement; a default box is used when absent.
    pub transform: Option<Transform>,
    /// Alternative text.
    pub alt: Option<String>,
}

/// One chart to add.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartElementInput {
    /// Chart family.
    pub chart_type: ChartType,
    /// Editable data.
    pub data: ChartData,
    /// Placement; a default box is used when absent.
    pub transform: Option<Transform>,
    /// Required alternative text.
    pub alt: String,
}

/// One formula to add.
#[derive(Debug, Clone, PartialEq)]
pub struct FormulaElementInput {
    /// Source expression.
    pub expression: String,
    /// Placement; a default box is used when absent.
    pub transform: Option<Transform>,
    /// Alternative text.
    pub alt: Option<String>,
}

impl Deck {
    /// Runs `work` atomically against a draft of the current revision.
    ///
    /// `expected_revision` rejects the edit when the caller worked from an older
    /// revision. A failed closure drops the draft without consuming a revision.
    pub fn transaction<T>(
        &mut self,
        expected_revision: Option<u64>,
        work: impl FnOnce(&mut DeckEdit) -> Result<T, DeckError>,
    ) -> Result<T, DeckError> {
        if let Some(expected) = expected_revision
            && expected != self.revision()
        {
            return Err(DeckError::revision_conflict(self.revision(), Some(expected)).with_recoverable(true));
        }
        let mut edit = DeckEdit { document: self.document().clone() };
        let value = work(&mut edit)?;
        let mut document = edit.document;
        document.revision = self.revision() + 1;
        self.commit_document(document);
        Ok(value)
    }

    /// Restores the previous committed document.
    ///
    /// Undo is itself a commit: the revision stays monotonic so a playback or
    /// render session pinned to an older revision still detects the mismatch.
    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.undo_snapshot()
        else {
            return false;
        };
        let current = std::mem::replace(self.document_mut(), previous);
        self.document_mut().revision = current.revision + 1;
        self.push_redo(current);
        true
    }

    /// Reapplies the last undone document.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo_snapshot()
        else {
            return false;
        };
        let current = std::mem::replace(self.document_mut(), next);
        self.document_mut().revision = current.revision + 1;
        self.push_undo(current);
        true
    }
}

/// A draft revision produced by [`Deck::transaction`].
#[derive(Debug)]
pub struct DeckEdit {
    document: DeckDocument,
}

impl DeckEdit {
    /// Draft document, safe to read while editing.
    pub fn document(&self) -> &DeckDocument {
        &self.document
    }

    pub(super) fn document_mut(&mut self) -> &mut DeckDocument {
        &mut self.document
    }

    /// Adds a slide at the requested position.
    pub fn add_slide(&mut self, options: SlideAddOptions) -> Result<SlideId, DeckError> {
        if let Some(layout) = &options.layout {
            self.assert_layout(layout)?;
        }
        let index = options.index.unwrap_or(self.document.slides.len());
        if index > self.document.slides.len() {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, format!("slide index {index} is outside 0..={}", self.document.slides.len())));
        }
        let id = SlideId::new(self.allocate("slide")).expect("allocated identity is non-empty");
        self.document.slides.insert(
            index,
            DeckSlide { id: id.clone(), layout: options.layout, hidden: false, background: None, elements: Vec::new(), notes: None },
        );
        Ok(id)
    }

    /// Removes a slide.
    pub fn remove_slide(&mut self, id: &SlideId) -> Result<(), DeckError> {
        let index = self.slide_position(id)?;
        self.document.slides.remove(index);
        Ok(())
    }

    /// Reorders the deck; `order` must be a permutation of the current slides.
    pub fn reorder_slides(&mut self, order: &[SlideId]) -> Result<(), DeckError> {
        if order.len() != self.document.slides.len() {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, "the new order must list every slide exactly once"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for id in order {
            self.slide_position(id)?;
            if !seen.insert(id.as_str()) {
                return Err(DeckError::new(codes::ARGUMENT_INVALID, format!("slide `{id}` appears twice in the new order")));
            }
        }
        let mut slides = std::mem::take(&mut self.document.slides);
        let mut reordered = Vec::with_capacity(slides.len());
        for id in order {
            let index = slides.iter().position(|slide| &slide.id == id).expect("order was validated");
            reordered.push(slides.remove(index));
        }
        self.document.slides = reordered;
        Ok(())
    }

    /// Copies a slide with fresh identities for the slide and every element.
    pub fn duplicate_slide(&mut self, id: &SlideId, index: Option<usize>) -> Result<SlideId, DeckError> {
        let source = self.slide_position(id)?;
        let mut copy = self.document.slides[source].clone();
        let new_id = SlideId::new(self.allocate("slide")).expect("allocated identity is non-empty");
        copy.id = new_id.clone();
        for element in &mut copy.elements {
            element.id = ElementId::new(self.allocate("element")).expect("allocated identity is non-empty");
        }
        let insert = index.unwrap_or(source + 1);
        if insert > self.document.slides.len() {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, format!("slide index {insert} is outside 0..={}", self.document.slides.len())));
        }
        self.document.slides.insert(insert, copy);
        Ok(new_id)
    }

    /// Re-seats one slide in the deck order, keeping its identity and every element.
    ///
    /// The slide is detached first, so `index` counts the remaining slides;
    /// an index beyond the end places the slide last. Returns the final position.
    pub fn move_slide(&mut self, id: &SlideId, index: usize) -> Result<usize, DeckError> {
        let position = self.slide_position(id)?;
        let moved = self.document.slides.remove(position);
        let index = index.min(self.document.slides.len());
        self.document.slides.insert(index, moved);
        Ok(index)
    }

    /// Swaps the positions of two slides, keeping every identity.
    pub fn swap_slides(&mut self, first: &SlideId, second: &SlideId) -> Result<(), DeckError> {
        let from = self.slide_position(first)?;
        let to = self.slide_position(second)?;
        self.document.slides.swap(from, to);
        Ok(())
    }

    /// Applies or clears a slide's layout, rejecting unknown layouts.
    pub fn set_slide_layout(&mut self, id: &SlideId, layout: Option<LayoutId>) -> Result<(), DeckError> {
        if let Some(candidate) = &layout {
            self.assert_layout(candidate)?;
        }
        self.slide_mut(id)?.layout = layout;
        Ok(())
    }

    /// Sets or clears a slide's background override.
    pub fn set_slide_background(&mut self, id: &SlideId, fill: Option<Fill>) -> Result<(), DeckError> {
        self.slide_mut(id)?.background = fill;
        Ok(())
    }

    /// Sets or clears the hidden flag.
    pub fn set_slide_hidden(&mut self, id: &SlideId, hidden: bool) -> Result<(), DeckError> {
        self.slide_mut(id)?.hidden = hidden;
        Ok(())
    }

    /// Sets or clears speaker notes.
    pub fn set_slide_notes(&mut self, id: &SlideId, notes: Option<String>) -> Result<(), DeckError> {
        self.slide_mut(id)?.notes = notes;
        Ok(())
    }

    /// Sets or clears the deck title, rejecting a blank title.
    ///
    /// `None` clears it, so a deck may stay untitled. The accepted title is
    /// trimmed once here, and the stored value is what the package writer emits.
    pub fn set_deck_title(&mut self, title: Option<String>) -> Result<(), DeckError> {
        if let Some(value) = &title
            && value.trim().is_empty()
        {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, "the deck title must not be blank; pass none to clear it"));
        }
        self.document.title = title.map(|value| value.trim().to_string());
        Ok(())
    }

    /// Sets the slide extent, rejecting non-positive dimensions.
    ///
    /// Existing element boxes are not rescaled: placement belongs to the elements,
    /// so shrinking the deck cannot silently move authored content.
    pub fn set_deck_size(&mut self, size: DeckSize) -> Result<(), DeckError> {
        if size.width_emu <= 0 || size.height_emu <= 0 {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, "deck width and height must be positive"));
        }
        self.document.size = size;
        Ok(())
    }

    /// Writes the title placeholder, creating it when the slide has none.
    pub fn set_slide_title(&mut self, id: &SlideId, text: impl Into<String>) -> Result<ElementId, DeckError> {
        let text = TextContent::plain(text);
        let layout = self.slide_mut(id)?.layout.clone();
        if let Some(element) = self.slide_mut(id)?.elements.iter_mut().find(|element| {
            element.kind == ElementKind::Text && element.placeholder.as_ref().is_some_and(|placeholder| placeholder.name == "title")
        }) {
            element.content = ElementContent::Text(text);
            return Ok(element.id.clone());
        }
        let transform = self.default_transform(DefaultBox::Title);
        let mut element = Element::new(
            ElementId::new(self.allocate("element")).expect("allocated identity is non-empty"),
            transform,
            ElementContent::Text(text),
        );
        element.placeholder = Some(PlaceholderRef { layout, name: "title".to_string() });
        element.source = Some(ElementOrigin { kind: OriginKind::Authoring, part: None });
        let element_id = element.id.clone();
        self.slide_mut(id)?.elements.push(element);
        Ok(element_id)
    }

    /// Adds a text element.
    pub fn add_text(&mut self, slide: &SlideId, input: TextElementInput) -> Result<ElementId, DeckError> {
        let transform = input.transform.unwrap_or_else(|| self.default_transform(DefaultBox::Body));
        assert_positive(&transform)?;
        let mut element = Element::new(
            ElementId::new(self.allocate("element")).expect("allocated identity is non-empty"),
            transform,
            ElementContent::Text(input.text),
        );
        element.placeholder = input.placeholder;
        element.alt = input.alt;
        element.source = Some(ElementOrigin { kind: OriginKind::Authoring, part: None });
        self.push_element(slide, element)
    }

    /// Adds an image element bound to an existing asset.
    pub fn add_image(&mut self, slide: &SlideId, input: ImageElementInput) -> Result<ElementId, DeckError> {
        if input.alt.trim().is_empty() {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, "image elements require alternative text"));
        }
        self.assert_asset(&input.asset)?;
        let transform = input.transform.unwrap_or_else(|| self.default_transform(DefaultBox::Body));
        assert_positive(&transform)?;
        let mut element = Element::new(
            ElementId::new(self.allocate("element")).expect("allocated identity is non-empty"),
            transform,
            ElementContent::Image(ImageContent { asset: input.asset, crop: None }),
        );
        element.alt = Some(input.alt);
        element.source = Some(ElementOrigin { kind: OriginKind::Authoring, part: None });
        self.push_element(slide, element)
    }

    /// Adds a preset shape.
    pub fn add_shape(&mut self, slide: &SlideId, input: ShapeElementInput) -> Result<ElementId, DeckError> {
        if input.geometry.trim().is_empty() {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, "shape geometry must not be empty"));
        }
        let transform = input.transform.unwrap_or_else(|| self.default_transform(DefaultBox::Body));
        assert_positive(&transform)?;
        let mut element = Element::new(
            ElementId::new(self.allocate("element")).expect("allocated identity is non-empty"),
            transform,
            ElementContent::Shape(ShapeContent { geometry: input.geometry, fill: input.fill, line: input.line }),
        );
        element.alt = input.alt;
        element.style = ElementStyle::default();
        element.source = Some(ElementOrigin { kind: OriginKind::Authoring, part: None });
        self.push_element(slide, element)
    }

    /// Adds a table whose rows share one cell count.
    pub fn add_table(&mut self, slide: &SlideId, input: TableElementInput) -> Result<ElementId, DeckError> {
        let columns = input.column_widths_emu.len();
        if columns == 0 || input.rows.is_empty() {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, "tables need at least one column and one row"));
        }
        for row in &input.rows {
            if row.cells.len() != columns {
                return Err(DeckError::new(codes::ARGUMENT_INVALID, "table rows must share the same column count"));
            }
            if row.height_emu <= 0 {
                return Err(DeckError::new(codes::ARGUMENT_INVALID, "table row heights must be positive"));
            }
        }
        let transform = input.transform.unwrap_or_else(|| self.default_transform(DefaultBox::Body));
        assert_positive(&transform)?;
        let mut element = Element::new(
            ElementId::new(self.allocate("element")).expect("allocated identity is non-empty"),
            transform,
            ElementContent::Table(TableContent { column_widths_emu: input.column_widths_emu, rows: input.rows }),
        );
        element.alt = input.alt;
        element.source = Some(ElementOrigin { kind: OriginKind::Authoring, part: None });
        self.push_element(slide, element)
    }

    /// Adds a chart kept as editable data.
    pub fn add_chart(&mut self, slide: &SlideId, input: ChartElementInput) -> Result<ElementId, DeckError> {
        if input.alt.trim().is_empty() {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, "chart elements require alternative text"));
        }
        let categories = input.data.categories.len();
        if categories == 0 || input.data.series.is_empty() {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, "charts need at least one category and one series"));
        }
        for series in &input.data.series {
            if series.values.len() != categories {
                return Err(DeckError::new(
                    codes::ARGUMENT_INVALID,
                    format!("series `{}` has {} values for {categories} categories", series.name, series.values.len()),
                ));
            }
        }
        let transform = input.transform.unwrap_or_else(|| self.default_transform(DefaultBox::Wide));
        assert_positive(&transform)?;
        let mut element = Element::new(
            ElementId::new(self.allocate("element")).expect("allocated identity is non-empty"),
            transform,
            ElementContent::Chart(ChartContent { chart_type: input.chart_type, data: input.data }),
        );
        element.alt = Some(input.alt);
        element.source = Some(ElementOrigin { kind: OriginKind::Authoring, part: None });
        self.push_element(slide, element)
    }

    /// Adds a formula keeping its source expression.
    pub fn add_formula(&mut self, slide: &SlideId, input: FormulaElementInput) -> Result<ElementId, DeckError> {
        if input.expression.trim().is_empty() {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, "formula expression must not be empty"));
        }
        let transform = input.transform.unwrap_or_else(|| self.default_transform(DefaultBox::Body));
        assert_positive(&transform)?;
        let mut element = Element::new(
            ElementId::new(self.allocate("element")).expect("allocated identity is non-empty"),
            transform,
            ElementContent::Formula(FormulaContent { expression: input.expression }),
        );
        element.alt = input.alt;
        element.source = Some(ElementOrigin { kind: OriginKind::Authoring, part: None });
        self.push_element(slide, element)
    }

    /// Adds a media element bound to an existing asset.
    pub fn add_media(
        &mut self,
        slide: &SlideId,
        media_kind: MediaKind,
        asset: AssetId,
        alt: impl Into<String>,
        transform: Option<Transform>,
    ) -> Result<ElementId, DeckError> {
        let alt = alt.into();
        if alt.trim().is_empty() {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, "media elements require alternative text"));
        }
        self.assert_asset(&asset)?;
        let transform = transform.unwrap_or_else(|| self.default_transform(DefaultBox::Wide));
        assert_positive(&transform)?;
        let mut element = Element::new(
            ElementId::new(self.allocate("element")).expect("allocated identity is non-empty"),
            transform,
            ElementContent::Media(MediaContent { media_kind, asset }),
        );
        element.alt = Some(alt);
        element.source = Some(ElementOrigin { kind: OriginKind::Authoring, part: None });
        self.push_element(slide, element)
    }

    /// Updates the placement of one element.
    pub fn update_transform(&mut self, slide: &SlideId, element: &ElementId, transform: Transform) -> Result<(), DeckError> {
        assert_positive(&transform)?;
        self.element_mut(slide, element)?.transform = transform;
        Ok(())
    }

    /// Updates alternative text.
    pub fn update_alt(&mut self, slide: &SlideId, element: &ElementId, alt: Option<String>) -> Result<(), DeckError> {
        self.element_mut(slide, element)?.alt = alt;
        Ok(())
    }

    /// Replaces the text of a text element.
    pub fn set_element_text(&mut self, slide: &SlideId, element: &ElementId, text: TextContent) -> Result<(), DeckError> {
        let target = self.element_mut(slide, element)?;
        match &mut target.content {
            ElementContent::Text(content) => {
                *content = text;
                Ok(())
            }
            _ => Err(DeckError::new(codes::OBJECT_TYPE_MISMATCH, "only text elements accept text")
                .with_slide(slide.clone())
                .with_element(element.clone())),
        }
    }

    /// Replaces the editable data of a chart element.
    pub fn set_chart_data(&mut self, slide: &SlideId, element: &ElementId, data: ChartData) -> Result<(), DeckError> {
        let categories = data.categories.len();
        for series in &data.series {
            if series.values.len() != categories {
                return Err(DeckError::new(codes::ARGUMENT_INVALID, "series values must align with categories"));
            }
        }
        let target = self.element_mut(slide, element)?;
        match &mut target.content {
            ElementContent::Chart(content) => {
                content.data = data;
                Ok(())
            }
            _ => Err(DeckError::new(codes::OBJECT_TYPE_MISMATCH, "only chart elements accept chart data")
                .with_slide(slide.clone())
                .with_element(element.clone())),
        }
    }

    /// Sets lock flags.
    pub fn lock_element(&mut self, slide: &SlideId, element: &ElementId, flags: LockFlags) -> Result<(), DeckError> {
        self.element_mut(slide, element)?.lock = Some(flags);
        Ok(())
    }

    /// Removes one element.
    pub fn remove_element(&mut self, slide: &SlideId, element: &ElementId) -> Result<(), DeckError> {
        let target = self.slide_mut(slide)?;
        let index = target.elements.iter().position(|candidate| &candidate.id == element).ok_or_else(|| {
            DeckError::unknown(ObjectKind::Element, element.as_str())
                .with_slide(slide.clone())
                .with_element(element.clone())
        })?;
        target.elements.remove(index);
        Ok(())
    }

    /// Embeds asset bytes and records their SHA-256.
    pub fn add_asset(
        &mut self,
        name: impl Into<String>,
        media_type: impl Into<String>,
        bytes: Vec<u8>,
        license: Option<String>,
    ) -> Result<AssetId, DeckError> {
        if bytes.is_empty() {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, "asset bytes must not be empty"));
        }
        let sha256 = sha256_hex(&bytes);
        let id = AssetId::new(self.allocate("asset")).expect("allocated identity is non-empty");
        self.document.assets.push(Asset { id: id.clone(), name: name.into(), media_type: media_type.into(), sha256, license, bytes });
        Ok(id)
    }

    fn push_element(&mut self, slide: &SlideId, element: Element) -> Result<ElementId, DeckError> {
        let element_id = element.id.clone();
        self.slide_mut(slide)?.elements.push(element);
        Ok(element_id)
    }

    fn slide_position(&self, id: &SlideId) -> Result<usize, DeckError> {
        self.document
            .slides
            .iter()
            .position(|slide| &slide.id == id)
            .ok_or_else(|| DeckError::unknown(ObjectKind::Slide, id.as_str()))
    }

    pub(super) fn slide_mut(&mut self, id: &SlideId) -> Result<&mut DeckSlide, DeckError> {
        let index = self.slide_position(id)?;
        Ok(&mut self.document.slides[index])
    }

    fn element_mut(&mut self, slide: &SlideId, element: &ElementId) -> Result<&mut Element, DeckError> {
        let slide = self.slide_mut(slide)?;
        slide.element_mut(element).ok_or_else(|| {
            DeckError::unknown(ObjectKind::Element, element.as_str()).with_element(element.clone())
        })
    }

    fn assert_layout(&self, layout: &LayoutId) -> Result<(), DeckError> {
        if self.document.layouts.iter().any(|candidate| &candidate.id == layout) {
            Ok(())
        }
        else {
            Err(DeckError::unknown(ObjectKind::Layout, layout.as_str()))
        }
    }

    pub(super) fn assert_asset(&self, asset: &AssetId) -> Result<(), DeckError> {
        if self.document.assets.iter().any(|candidate| &candidate.id == asset) {
            Ok(())
        }
        else {
            Err(DeckError::asset_missing(asset.as_str()))
        }
    }

    pub(super) fn allocate(&mut self, prefix: &str) -> String {
        self.document.next_object_id.allocate(prefix)
    }

    fn default_transform(&self, kind: DefaultBox) -> Transform {
        let size = self.document.size;
        let (x, y, width, height) = match kind {
            DefaultBox::Title => (size.width_emu / 20, size.height_emu / 20, size.width_emu * 9 / 10, size.height_emu / 5),
            DefaultBox::Body => (size.width_emu / 10, size.height_emu * 3 / 10, size.width_emu * 8 / 10, size.height_emu / 2),
            DefaultBox::Wide => (size.width_emu / 10, size.height_emu * 3 / 10, size.width_emu * 8 / 10, size.height_emu * 11 / 20),
        };
        Transform { x_emu: x, y_emu: y, width_emu: width, height_emu: height, rotation_deg: None }
    }
}

/// Default placement families used when the caller passes no transform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultBox {
    /// Top title band.
    Title,
    /// Centered body block.
    Body,
    /// Wider block for charts and media.
    Wide,
}

fn assert_positive(transform: &Transform) -> Result<(), DeckError> {
    if transform.width_emu <= 0 || transform.height_emu <= 0 {
        return Err(DeckError::new(codes::ARGUMENT_INVALID, "element width and height must be positive"));
    }
    Ok(())
}

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}
