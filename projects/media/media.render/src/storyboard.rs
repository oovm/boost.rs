//! Storyboard and contact-sheet pages composed from the film's own frames.
//!
//! Every image here comes from [`crate::render::render_frame_at`], the same
//! painter a preview and an export use, and every label is drawn with the
//! project's own fonts. A storyboard page is therefore a view of the film, not
//! a second interpretation of it: nothing is re-rendered by a host tool and no
//! frame is pasted from a screenshot.
//!
//! Layout is a plain grid over [`Frame`] primitives. Panel identity, shot and
//! frame range come from [`media_scene::Production`], so a page can be mapped
//! back to the exact panel a note refers to.

use std::path::{Path, PathBuf};

use geometry::Rgba;
use media_image::{Bitmap, Fit, Frame};
use media_raster::Paint;
use media_scene::{Production, Project, Scene};
use media_text::{FontId, Fonts};
use media_types::Error;

use crate::render::{RenderStats, render_frame_at, resolve_fonts};

/// Page geometry and type scale for storyboard output.
#[derive(Clone, Debug)]
pub struct StoryboardOptions {
    /// Page width in pixels.
    pub width: u32,
    /// Page height in pixels.
    pub height: u32,
    /// Thumbnail columns. Storyboard rows always carry three key frames, so a
    /// value below three is raised to three.
    pub columns: u32,
    /// Panel rows per page. Each row is one panel.
    pub rows: u32,
    /// Page margin in pixels.
    pub margin: f32,
    /// Gap between thumbnails and rows.
    pub gap: f32,
    /// Title size.
    pub title_size: f32,
    /// Caption size.
    pub label_size: f32,
    /// Page paper color.
    pub background: [u8; 3],
    /// Text color.
    pub ink: [u8; 3],
}

impl Default for StoryboardOptions {
    fn default() -> Self {
        Self {
            width: 1600,
            height: 1200,
            columns: 3,
            rows: 3,
            margin: 56.0,
            gap: 24.0,
            title_size: 34.0,
            label_size: 19.0,
            background: [250, 250, 252],
            ink: [28, 32, 40],
        }
    }
}

/// One panel placed on a page.
#[derive(Clone, Debug)]
pub struct StoryboardCell {
    /// Shot the panel belongs to.
    pub shot: String,
    /// Panel handle.
    pub panel: String,
    /// Absolute project frames drawn for this panel, in order.
    pub frames: Vec<u32>,
    /// Line spoken during the panel, when one is declared.
    pub dialogue: Option<String>,
}

/// One written page.
#[derive(Clone, Debug)]
pub struct StoryboardPage {
    /// Zero-based page order.
    pub index: usize,
    /// Written file path.
    pub path: String,
    /// Panels on this page.
    pub cells: Vec<StoryboardCell>,
}

/// A panel resolved against the film's timeline.
struct PanelBlock {
    shot: String,
    panel: String,
    frames: [u32; 3],
    purpose: String,
    composition: String,
    dialogue: Option<String>,
    camera: Option<String>,
    sound: Option<String>,
    safe_area: Option<[f32; 4]>,
    /// Absolute start and end of the panel, for the timeline page.
    span: [u32; 2],
}

/// Writes the storyboard, one page per `options.rows` panels.
///
/// Each panel row carries three key frames (first, middle, last) with its shot,
/// handle and frame range, plus the declared purpose, composition and dialogue.
/// A production that fails [`Production::validate`] is refused before any page
/// is written, so a broken reference cannot produce a silently empty sheet.
pub fn write_storyboard(
    json: &str,
    production: &Production,
    out_dir: &Path,
    options: &StoryboardOptions,
) -> Result<Vec<StoryboardPage>, Error> {
    write_storyboard_selected(json, production, out_dir, options, &[])
}

/// Writes only the pages that contain one of `only_panels`.
///
/// Page numbering stays stable: a filtered run keeps the index the full book
/// would have given that page, so a review note pointing at page 3 keeps
/// pointing at page 3 after an edit re-renders it. An empty filter writes the
/// whole book.
pub fn write_storyboard_selected(
    json: &str,
    production: &Production,
    out_dir: &Path,
    options: &StoryboardOptions,
    only_panels: &[String],
) -> Result<Vec<StoryboardPage>, Error> {
    production.validate()?;
    let project = parse_project(json)?;
    let mut fonts = fonts_for(&project)?;
    let blocks = plan_blocks(&project, production)?;
    if blocks.is_empty() {
        return Err(Error::invalid("the production declares no storyboard panel"));
    }
    std::fs::create_dir_all(out_dir).map_err(|e| Error::io(format!("cannot create {}: {e}", out_dir.display())))?;

    let per_page = options.rows.max(1) as usize;
    let page_count = blocks.len().div_ceil(per_page);
    let mut pages = Vec::new();
    for (page_index, chunk) in blocks.chunks(per_page).enumerate() {
        if !only_panels.is_empty() && !chunk.iter().any(|block| only_panels.iter().any(|panel| panel == &block.panel)) {
            continue;
        }
        let path = out_dir.join(format!("storyboard-{:03}.png", page_index + 1));
        let mut page = Frame::new(options.width, options.height);
        page.fill(rgba(options.background, 255));
        draw_title(&mut fonts, &mut page, options, &format!("Storyboard {} / {page_count}", page_index + 1));

        let rows = chunk.len() as u32;
        let content_top = options.margin + options.title_size + options.gap;
        let content_height = options.height as f32 - content_top - options.margin;
        let row_height = (content_height - options.gap * (rows.saturating_sub(1)) as f32) / rows.max(1) as f32;

        for (row, block) in chunk.iter().enumerate() {
            let top = content_top + (row_height + options.gap) * row as f32;
            draw_panel_row(&mut fonts, &mut page, options, json, block, top, row_height)?;
        }

        write_page(&path, &page)?;
        pages.push(StoryboardPage {
            index: page_index,
            path: path.to_string_lossy().into_owned(),
            cells: chunk
                .iter()
                .map(|block| StoryboardCell {
                    shot: block.shot.clone(),
                    panel: block.panel.clone(),
                    frames: block.frames.to_vec(),
                    dialogue: block.dialogue.clone(),
                })
                .collect(),
        });
    }
    Ok(pages)
}

/// One before/after page.
#[derive(Clone, Debug)]
pub struct ComparisonPage {
    /// Written file path.
    pub path: String,
    /// Panels compared on this page, in order.
    pub panels: Vec<String>,
}

/// Writes a before/after page: each panel's middle frame from two revisions.
///
/// Both revisions are rendered with the same painter, so a colour or layout
/// change shows as the real difference instead of a re-encode artifact. The
/// panel set and layout come from the current book; a panel that only exists in
/// `before` is not shown, because the review is about the current storyboard.
pub fn write_comparison(
    before_json: &str,
    after_json: &str,
    production: &Production,
    out: &Path,
    only_panels: &[String],
    options: &StoryboardOptions,
) -> Result<ComparisonPage, Error> {
    production.validate()?;
    let before = parse_project(before_json)?;
    let after = parse_project(after_json)?;
    let mut fonts = fonts_for(&after)?;
    let blocks = plan_blocks(&after, production)?;
    let selected: Vec<&PanelBlock> = blocks
        .iter()
        .filter(|block| only_panels.is_empty() || only_panels.iter().any(|panel| panel == &block.panel))
        .collect();
    if selected.is_empty() {
        return Err(Error::invalid("the production declares no panel to compare"));
    }

    let mut page = Frame::new(options.width, options.height);
    page.fill(rgba(options.background, 255));
    draw_title(&mut fonts, &mut page, options, "Before / after");
    let top = options.margin + options.title_size + options.gap;
    let content_width = options.width as f32 - options.margin * 2.0;
    let row_height = (((options.height as f32 - top - options.margin - options.gap * (selected.len().saturating_sub(1)) as f32)
        / selected.len() as f32)
        .clamp(120.0, 340.0));
    let thumb_width = (content_width - options.gap) / 2.0;
    let label_height = options.label_size * 2.0;

    for (row, block) in selected.iter().enumerate() {
        let y = top + (row_height + options.gap) * row as f32;
        let image_height = (row_height - label_height).max(40.0);
        // A frame that only exists in the old revision still maps by the same
        // panel range, so the comparison stays on the same moment.
        let before_frame = block.frames[1].min(before.duration_frames.saturating_sub(1));
        let left = render_frame_at(before_json, before_frame)?;
        let right = render_frame_at(after_json, block.frames[1])?;
        page.draw_bitmap(&bitmap(left), options.margin, y, thumb_width, image_height, Fit::Contain, 1.0, 4.0);
        page.draw_bitmap(&bitmap(right), options.margin + thumb_width + options.gap, y, thumb_width, image_height, Fit::Contain, 1.0, 4.0);
        draw_text(&mut fonts, &mut page, options, "before", options.margin, y + image_height + options.label_size, options.label_size * 0.85, thumb_width);
        draw_text(
            &mut fonts,
            &mut page,
            options,
            "after",
            options.margin + thumb_width + options.gap,
            y + image_height + options.label_size,
            options.label_size * 0.85,
            thumb_width,
        );
        let header = format!("{} · {} · f{}", block.shot, block.panel, block.frames[1]);
        draw_text(&mut fonts, &mut page, options, &header, options.margin, y + image_height + options.label_size * 1.9, options.label_size, content_width);
    }

    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(format!("cannot create {}: {e}", parent.display())))?;
    }
    write_page(out, &page)?;
    Ok(ComparisonPage { path: out.to_string_lossy().into_owned(), panels: selected.iter().map(|block| block.panel.clone()).collect() })
}

/// Writes a chapter overview: one row per shot with its first and last frame.
///
/// The pair at the top of each row is also the seam check: the previous row's
/// last frame and this row's first frame are the two frames a cut hinges on.
pub fn write_chapters(
    json: &str,
    production: &Production,
    out: &Path,
    options: &StoryboardOptions,
) -> Result<StoryboardPage, Error> {
    production.validate()?;
    let project = parse_project(json)?;
    let mut fonts = fonts_for(&project)?;
    if production.shots.is_empty() {
        return Err(Error::invalid("the production declares no shot to list as a chapter"));
    }
    let mut page = Frame::new(options.width, options.height);
    page.fill(rgba(options.background, 255));
    draw_title(&mut fonts, &mut page, options, "Chapters");
    let top = options.margin + options.title_size + options.gap;
    let row_height = ((options.height as f32 - top - options.margin) / production.shots.len() as f32).clamp(110.0, 300.0);
    let label_width = 200.0;
    let content_width = options.width as f32 - options.margin * 2.0 - label_width;
    let thumb_width = (content_width - options.gap) / 2.0;
    let mut cells = Vec::new();

    for (row, shot) in production.shots.iter().enumerate() {
        let Some(scene) = scene_of(&project, &shot.scene) else {
            return Err(Error::invalid(format!("shot `{}` references unknown scene `{}`", shot.id, shot.scene)));
        };
        let y = top + row_height * row as f32;
        let image_height = (row_height - options.label_size * 1.6).max(40.0);
        let first = scene.start;
        let last = scene.start + scene.duration.saturating_sub(1);
        let left = render_frame_at(json, first)?;
        let right = render_frame_at(json, last)?;
        page.draw_bitmap(&bitmap(left), options.margin + label_width, y, thumb_width, image_height, Fit::Contain, 1.0, 4.0);
        page.draw_bitmap(&bitmap(right), options.margin + label_width + thumb_width + options.gap, y, thumb_width, image_height, Fit::Contain, 1.0, 4.0);
        let label = format!("{}", shot.id);
        draw_text(&mut fonts, &mut page, options, &label, options.margin, y + options.label_size, options.label_size, label_width);
        let timing = format!("{}–{} · {:.1}s", first, last, scene.duration as f32 / project.fps.max(1) as f32);
        draw_text(&mut fonts, &mut page, options, &timing, options.margin, y + options.label_size * 2.0, options.label_size * 0.85, label_width);
        draw_text(
            &mut fonts,
            &mut page,
            options,
            "first",
            options.margin + label_width,
            y + image_height + options.label_size,
            options.label_size * 0.8,
            thumb_width,
        );
        draw_text(
            &mut fonts,
            &mut page,
            options,
            "last",
            options.margin + label_width + thumb_width + options.gap,
            y + image_height + options.label_size,
            options.label_size * 0.8,
            thumb_width,
        );
        cells.push(StoryboardCell {
            shot: shot.id.to_string(),
            panel: String::new(),
            frames: vec![first, last],
            dialogue: None,
        });
    }

    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(format!("cannot create {}: {e}", parent.display())))?;
    }
    write_page(out, &page)?;
    Ok(StoryboardPage { index: 0, path: out.to_string_lossy().into_owned(), cells })
}

/// One scored cover candidate.
#[derive(Clone, Debug)]
pub struct CoverCandidate {
    /// Project frame the candidate was taken from.
    pub frame: u32,
    /// Written file path.
    pub path: String,
    /// Heuristic score: mean luminance plus contrast.
    pub score: f32,
}

/// Writes the strongest frames as cover candidates.
///
/// The score is a documented heuristic (mean luminance plus contrast), not an
/// aesthetic judgement: it exists to shortlist, and a human still picks. Frames
/// come from every panel's middle frame plus an even spread across the film, so
/// a candidate is always a frame the film actually shows.
pub fn write_cover_candidates(
    json: &str,
    production: &Production,
    out_dir: &Path,
    count: u32,
) -> Result<Vec<CoverCandidate>, Error> {
    production.validate()?;
    let project = parse_project(json)?;
    let blocks = plan_blocks(&project, production)?;
    if blocks.is_empty() {
        return Err(Error::invalid("the production declares no panel to sample for a cover"));
    }
    let mut frames: Vec<u32> = blocks.iter().map(|block| block.frames[1]).collect();
    let total = project.duration_frames.max(1);
    for step in 1..=8u32 {
        frames.push(total.saturating_mul(step) / 9);
    }
    frames.sort_unstable();
    frames.dedup();

    let mut scored: Vec<(f32, u32, Frame)> = Vec::new();
    for frame in frames {
        let raster = render_frame_at(json, frame)?;
        scored.push((cover_score(&raster), frame, raster));
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    std::fs::create_dir_all(out_dir).map_err(|e| Error::io(format!("cannot create {}: {e}", out_dir.display())))?;
    let take = count.clamp(1, scored.len() as u32) as usize;
    let mut candidates = Vec::with_capacity(take);
    for (index, (score, frame, raster)) in scored.into_iter().take(take).enumerate() {
        let path = out_dir.join(format!("cover-{:02}.png", index + 1));
        write_page(&path, &raster)?;
        candidates.push(CoverCandidate { frame, path: path.to_string_lossy().into_owned(), score });
    }
    Ok(candidates)
}

/// Mean luminance plus contrast, both in `0..=1`, weighted for a shortlist.
fn cover_score(frame: &Frame) -> f32 {
    let pixels = (frame.width * frame.height).max(1) as f32;
    let mut sum = 0.0f32;
    let mut sum_sq = 0.0f32;
    for chunk in frame.pixels.chunks_exact(4) {
        let luma = (0.2126 * chunk[0] as f32 + 0.7152 * chunk[1] as f32 + 0.0722 * chunk[2] as f32) / 255.0;
        sum += luma;
        sum_sq += luma * luma;
    }
    let mean = sum / pixels;
    let variance = (sum_sq / pixels - mean * mean).max(0.0);
    0.6 * mean + 0.4 * variance.sqrt()
}

/// Writes one contact sheet: the middle frame of every panel in a grid.
pub fn write_contact_sheet(
    json: &str,
    production: &Production,
    out: &Path,
    options: &StoryboardOptions,
) -> Result<StoryboardPage, Error> {
    production.validate()?;
    let project = parse_project(json)?;
    let mut fonts = fonts_for(&project)?;
    let blocks = plan_blocks(&project, production)?;
    if blocks.is_empty() {
        return Err(Error::invalid("the production declares no storyboard panel"));
    }
    let columns = options.columns.max(1);
    let rows = options.rows.max(1);
    let mut page = Frame::new(options.width, options.height);
    page.fill(rgba(options.background, 255));
    draw_title(&mut fonts, &mut page, options, "Contact sheet");

    let content_top = options.margin + options.title_size + options.gap;
    let cell_width = (options.width as f32 - options.margin * 2.0 - options.gap * (columns - 1) as f32) / columns as f32;
    let cell_height = (options.height as f32 - content_top - options.margin - options.gap * (rows - 1) as f32) / rows as f32;
    let mut cells = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let slot = index as u32;
        if slot >= columns * rows {
            break;
        }
        let (col, row) = (slot % columns, slot / columns);
        let x = options.margin + (cell_width + options.gap) * col as f32;
        let y = content_top + (cell_height + options.gap) * row as f32;
        let frame = render_frame_at(json, block.frames[1])?;
        let image_height = cell_height - options.label_size * 2.6;
        page.draw_bitmap(&bitmap(frame), x, y, cell_width, image_height, Fit::Contain, 1.0, 4.0);
        let baseline = y + image_height + options.label_size * 1.2;
        let label = format!("{} · {}", block.shot, block.panel);
        draw_text(&mut fonts, &mut page, options, &label, x, baseline, options.label_size, cell_width);
        cells.push(StoryboardCell {
            shot: block.shot.clone(),
            panel: block.panel.clone(),
            frames: vec![block.frames[1]],
            dialogue: block.dialogue.clone(),
        });
    }
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(format!("cannot create {}: {e}", parent.display())))?;
    }
    write_page(out, &page)?;
    Ok(StoryboardPage { index: 0, path: out.to_string_lossy().into_owned(), cells })
}

fn parse_project(json: &str) -> Result<Project, Error> {
    serde_json::from_str(json).map_err(|e| Error::invalid(format!("project json: {e}")))
}

fn fonts_for(project: &Project) -> Result<Fonts, Error> {
    resolve_fonts(project).map(|(fonts, _, _)| fonts)
}

/// Resolves every panel to absolute project frames, in storyboard order.
fn plan_blocks(project: &Project, production: &Production) -> Result<Vec<PanelBlock>, Error> {
    let mut blocks = Vec::new();
    for shot in &production.shots {
        let Some(scene) = scene_of(project, &shot.scene) else {
            return Err(Error::invalid(format!("shot `{}` references unknown scene `{}`", shot.id, shot.scene)));
        };
        for panel in &shot.panels {
            let [start, end] = panel.range;
            // Local frames inside the shot, then shifted to absolute project
            // frames. A panel never straddles its scene: `validate` guarantees
            // the range fits the scene duration.
            let middle = start + (end - start) / 2;
            let frames = [scene.start + start, scene.start + middle, scene.start + end - 1];
            blocks.push(PanelBlock {
                shot: shot.id.to_string(),
                panel: panel.id.to_string(),
                frames,
                purpose: panel.purpose.clone(),
                composition: panel.composition.clone(),
                dialogue: panel.dialogue.clone(),
                camera: panel.camera.clone(),
                sound: panel.sound.clone(),
                safe_area: panel.safe_area,
                span: [scene.start + start, scene.start + end],
            });
        }
    }
    Ok(blocks)
}

fn scene_of<'a>(project: &'a Project, id: &str) -> Option<&'a Scene> {
    project.scenes.iter().find(|scene| scene.id.as_deref() == Some(id))
}

fn draw_title(fonts: &mut Fonts, page: &mut Frame, options: &StoryboardOptions, title: &str) {
    fonts.draw_line(page, title, options.margin, options.margin + options.title_size, options.title_size, FontId::Bold, rgba(options.ink, 255));
}

#[allow(clippy::too_many_arguments)]
fn draw_panel_row(
    fonts: &mut Fonts,
    page: &mut Frame,
    options: &StoryboardOptions,
    json: &str,
    block: &PanelBlock,
    top: f32,
    row_height: f32,
) -> Result<(), Error> {
    let content_width = options.width as f32 - options.margin * 2.0;
    let caption_height = options.label_size * 4.6;
    let thumb_height = (row_height - caption_height).max(options.label_size * 3.0);
    // One panel row always carries exactly three key frames.
    let columns = 3;
    let thumb_width = (content_width - options.gap * (columns - 1) as f32) / columns as f32;

    for (index, frame_index) in block.frames.iter().enumerate() {
        let x = options.margin + (thumb_width + options.gap) * index as f32;
        let frame = render_frame_at(json, *frame_index)?;
        let (source_width, source_height) = (frame.width as f32, frame.height as f32);
        page.round_rect(x, top, thumb_width, thumb_height, 4.0, rgba([18, 22, 30], 255));
        page.draw_bitmap(&bitmap(frame), x, top, thumb_width, thumb_height, Fit::Contain, 1.0, 4.0);
        if index == 0 {
            if let Some(area) = block.safe_area {
                // The safe area is declared against the picture, so it maps to
                // the contained image box, not the padded thumbnail cell.
                let scale = (thumb_width / source_width).min(thumb_height / source_height);
                let image_width = source_width * scale;
                let image_height = source_height * scale;
                let box_rect = [
                    x + (thumb_width - image_width) / 2.0,
                    top + (thumb_height - image_height) / 2.0,
                    image_width,
                    image_height,
                ];
                draw_safe_area(page, area, box_rect);
            }
        }
        let tag = match index {
            0 => "first",
            1 => "middle",
            _ => "last",
        };
        let caption = format!("{tag} · f{}", frame_index);
        draw_text(fonts, page, options, &caption, x, top + thumb_height + options.label_size, options.label_size * 0.85, thumb_width);
    }

    let baseline = top + thumb_height + options.label_size * 2.1;
    let header = format!("{} · {} · frames {}..{}", block.shot, block.panel, block.span[0], block.span[1]);
    draw_text(fonts, page, options, &header, options.margin, baseline, options.label_size * 1.15, content_width);
    let detail = match &block.dialogue {
        Some(line) => format!("{} / {} / \u{201c}{line}\u{201d}", block.purpose, block.composition),
        None => format!("{} / {}", block.purpose, block.composition),
    };
    draw_text(fonts, page, options, &detail, options.margin, baseline + options.label_size * 1.5, options.label_size, content_width);
    let notes: Vec<String> = [
        block.camera.as_ref().map(|camera| format!("camera: {camera}")),
        block.sound.as_ref().map(|sound| format!("sound: {sound}")),
        block.safe_area.map(|_| "safe area: outlined".to_owned()),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !notes.is_empty() {
        draw_text(
            fonts,
            page,
            options,
            &notes.join("  ·  "),
            options.margin,
            baseline + options.label_size * 2.9,
            options.label_size * 0.9,
            content_width,
        );
    }
    Ok(())
}

/// Writes a timeline overview: one row per shot, panel ticks inside the bar and
/// cue ticks in a strip below.
///
/// The page is a projection of the same book the storyboard uses, so a row maps
/// back to `Shot.id` and every tick to a `Panel.id` or `Cue.id`.
pub fn write_timeline(
    json: &str,
    production: &Production,
    out: &Path,
    options: &StoryboardOptions,
) -> Result<StoryboardPage, Error> {
    production.validate()?;
    let project = parse_project(json)?;
    let mut fonts = fonts_for(&project)?;
    if production.shots.is_empty() {
        return Err(Error::invalid("the production declares no shot to place on a timeline"));
    }
    let mut page = Frame::new(options.width, options.height);
    page.fill(rgba(options.background, 255));
    draw_title(&mut fonts, &mut page, options, "Timeline");

    let total = project.duration_frames.max(1) as f32;
    let label_width = 190.0;
    let left = options.margin + label_width;
    let right = options.width as f32 - options.margin;
    let top = options.margin + options.title_size + options.gap * 2.0;
    let row_height = 54.0;
    let row_gap = 12.0;
    let bar = rgba([122, 162, 255], 255);
    let tick = rgba([20, 26, 36], 255);
    let mut cells = Vec::new();

    for (row, shot) in production.shots.iter().enumerate() {
        let Some(scene) = scene_of(&project, &shot.scene) else {
            return Err(Error::invalid(format!("shot `{}` references unknown scene `{}`", shot.id, shot.scene)));
        };
        let y = top + row as f32 * (row_height + row_gap);
        let x0 = left + (scene.start as f32 / total) * (right - left);
        let x1 = left + ((scene.start + scene.duration) as f32 / total) * (right - left);
        page.round_rect(x0, y, (x1 - x0).max(2.0), row_height, 6.0, bar);
        draw_text(&mut fonts, &mut page, options, shot.id.as_str(), options.margin, y + row_height * 0.68, options.label_size, label_width);

        for panel in &shot.panels {
            let px = left + ((scene.start + panel.range[0]) as f32 / total) * (right - left);
            page.rect(px, y, 1.5, row_height, tick);
            let label = format!("{}", panel.id);
            draw_text(
                &mut fonts,
                &mut page,
                options,
                &label,
                px + 3.0,
                y + options.label_size,
                options.label_size * 0.72,
                (x1 - px).max(20.0),
            );
        }
        cells.push(StoryboardCell {
            shot: shot.id.to_string(),
            panel: String::new(),
            frames: vec![scene.start, scene.start + scene.duration],
            dialogue: None,
        });
    }

    // Cue strip: one tick per cue at its absolute frame.
    let cue_top = top + production.shots.len() as f32 * (row_height + row_gap) + options.gap;
    let cue_height = 28.0;
    page.round_rect(left, cue_top, right - left, cue_height, 4.0, rgba([226, 232, 244], 255));
    for cue in &production.cues {
        let Some(shot) = production.shots.iter().find(|shot| shot.id == cue.shot) else {
            continue;
        };
        let Some(scene) = scene_of(&project, &shot.scene) else {
            continue;
        };
        let x = left + ((scene.start + cue.range[0]) as f32 / total) * (right - left);
        page.rect(x, cue_top, 2.0, cue_height, rgba([232, 184, 109], 255));
    }
    draw_text(
        &mut fonts,
        &mut page,
        options,
        "cues",
        options.margin,
        cue_top + cue_height * 0.72,
        options.label_size * 0.85,
        label_width,
    );
    let frames_note = format!("{} frames · {:.1}s", project.duration_frames, project.duration_frames as f32 / project.fps.max(1) as f32);
    draw_text(
        &mut fonts,
        &mut page,
        options,
        &frames_note,
        right - 260.0,
        cue_top + cue_height + options.label_size * 1.6,
        options.label_size * 0.85,
        260.0,
    );

    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(format!("cannot create {}: {e}", parent.display())))?;
    }
    write_page(out, &page)?;
    Ok(StoryboardPage { index: 0, path: out.to_string_lossy().into_owned(), cells })
}

/// Renders a low-detail motion preview of the whole film.
///
/// Dynamic storyboarding is the same project at a smaller raster, not a second
/// edit: the preview keeps every timing decision and drops resolution, so a
/// reviewer sees movement without paying for a master render.
pub fn write_motion_preview(
    json: &str,
    production: &Production,
    out: &Path,
    scale: f32,
    progress: &mut dyn FnMut(u32, u32),
) -> Result<RenderStats, Error> {
    production.validate()?;
    let mut project = parse_project(json)?;
    let scale = scale.clamp(0.1, 1.0);
    // H.264 wants even dimensions, so a scaled raster rounds up to the next
    // even value instead of failing the whole preview.
    let even = |value: f32| {
        let rounded = value.round().max(2.0) as u32;
        if rounded % 2 == 0 { rounded } else { rounded + 1 }
    };
    project.width = even(project.width as f32 * scale);
    project.height = even(project.height as f32 * scale);
    project.supersample = None;
    let json = serde_json::to_string(&project).map_err(|e| Error::invalid(format!("project json: {e}")))?;
    crate::render::render_project_with(&json, out, &crate::render::RenderOptions::default(), progress)
}

/// Draws a thin safe-area outline inside `box` (`[x, y, width, height]`).
fn draw_safe_area(page: &mut Frame, area: [f32; 4], box_rect: [f32; 4]) {
    let [bx, by, bw, bh] = box_rect;
    let [x0, y0, x1, y1] = area;
    let (left, top) = (bx + bw * x0, by + bh * y0);
    let (right, bottom) = (bx + bw * x1, by + bh * y1);
    let thickness = 2.0;
    let ink = Rgba([255, 214, 120, 220]);
    page.rect(left, top, right - left, thickness, ink);
    page.rect(left, bottom - thickness, right - left, thickness, ink);
    page.rect(left, top, thickness, bottom - top, ink);
    page.rect(right - thickness, top, thickness, bottom - top, ink);
}

/// Draws one line clipped to `max_width` with an ellipsis.
fn draw_text(fonts: &mut Fonts, page: &mut Frame, options: &StoryboardOptions, text: &str, x: f32, baseline: f32, size: f32, max_width: f32) {
    let mut clipped = text.to_owned();
    while fonts.measure(&clipped, size, false, false) > max_width && clipped.chars().count() > 4 {
        let mut chars: Vec<char> = clipped.chars().collect();
        chars.pop();
        chars.pop();
        clipped = chars.into_iter().collect::<String>() + "\u{2026}";
    }
    fonts.draw_line(page, &clipped, x, baseline, size, FontId::Regular, rgba(options.ink, 235));
}

fn bitmap(frame: Frame) -> Bitmap {
    Bitmap::new(frame.width, frame.height, frame.pixels)
}

fn write_page(path: &Path, page: &Frame) -> Result<(), Error> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(format!("cannot create {}: {e}", parent.display())))?;
    }
    media_codec::write_png(path, page.width, page.height, &page.pixels)
}

fn rgba(color: [u8; 3], alpha: u8) -> Rgba {
    Rgba([color[0], color[1], color[2], alpha])
}

#[cfg(test)]
mod tests {
    use media_scene::{Panel, PanelId, Shot, ShotId};

    use super::*;

    fn fixture() -> (String, Production) {
        let json = serde_json::json!({
            "width": 320, "height": 180, "fps": 30, "duration_frames": 120,
            "scenes": [
                { "id": "mechanism", "start": 0, "duration": 60,
                  "background": { "kind": "solid", "color": "#101820" }, "items": [] },
                { "id": "spatial", "start": 60, "duration": 60,
                  "background": { "kind": "solid", "color": "#203040" }, "items": [] }
            ],
            "audio": null
        })
        .to_string();
        let project: Project = serde_json::from_str(&json).expect("project");
        let mut production = Production::new(project);
        production.shots.push(Shot {
            id: ShotId::new("shot-mechanism").unwrap(),
            scene: "mechanism".to_owned(),
            locked: false,
            panels: vec![Panel {
                id: PanelId::new("panel-open").unwrap(),
                range: [0, 60],
                purpose: "show the request entering".to_owned(),
                composition: "node graph, left to right".to_owned(),
                dialogue: Some("先看请求怎么进来".to_owned()),
                camera: Some("push in 1.0 -> 1.2".to_owned()),
                sound: Some("typing starts".to_owned()),
                safe_area: Some([0.1, 0.1, 0.9, 0.85]),
                notes: Vec::new(),
            }],
        });
        production.shots.push(Shot {
            id: ShotId::new("shot-spatial").unwrap(),
            scene: "spatial".to_owned(),
            locked: false,
            panels: vec![Panel {
                id: PanelId::new("panel-room").unwrap(),
                range: [0, 30],
                purpose: "establish the room".to_owned(),
                composition: "wide, foreground props".to_owned(),
                dialogue: None,
                camera: None,
                sound: None,
                safe_area: None,
                notes: Vec::new(),
            }],
        });
        (json, production)
    }

    fn out_dir() -> PathBuf {
        let dir = std::env::temp_dir().join("opencut-storyboard-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn storyboard_pages_carry_every_panel_with_three_key_frames() {
        let (json, production) = fixture();
        let dir = out_dir();
        let pages = write_storyboard(&json, &production, &dir, &StoryboardOptions::default()).expect("storyboard");

        assert_eq!(pages.len(), 1, "two panels fit on one page");
        assert_eq!(pages[0].cells.len(), 2);
        let cell = &pages[0].cells[0];
        assert_eq!(cell.shot, "shot-mechanism");
        assert_eq!(cell.panel, "panel-open");
        assert_eq!(cell.frames, vec![0, 30, 59], "first, middle and last frame of the panel");
        assert_eq!(cell.dialogue.as_deref(), Some("先看请求怎么进来"));

        // The second shot starts at frame 60, so its panel maps to 60..90.
        assert_eq!(pages[0].cells[1].frames, vec![60, 75, 89]);

        let mut library = media_codec::MediaLibrary::new();
        let bitmap = library
            .image(&pages[0].path, media_codec::DecodeLimits::unbounded())
            .expect("decode page");
        assert_eq!((bitmap.width, bitmap.height), (1600, 1200));
    }

    #[test]
    fn contact_sheet_uses_one_frame_per_panel() {
        let (json, production) = fixture();
        let out = out_dir().join("contact.png");
        let sheet = write_contact_sheet(&json, &production, &out, &StoryboardOptions::default()).expect("contact sheet");
        assert_eq!(sheet.cells.len(), 2);
        assert_eq!(sheet.cells[0].frames, vec![30], "the middle frame represents the panel");
        assert!(out.exists());
    }

    #[test]
    fn a_broken_book_writes_no_page() {
        let (json, mut production) = fixture();
        production.shots[0].scene = "missing".to_owned();
        let dir = out_dir().join("broken");
        let error = write_storyboard(&json, &production, &dir, &StoryboardOptions::default()).expect_err("broken reference must fail");
        assert!(error.message.contains("unknown scene `missing`"), "{}", error.message);
        assert!(!dir.exists(), "nothing is written for a broken book");
    }

    #[test]
    fn timeline_places_every_shot_and_cue() {
        let (json, production) = fixture();
        let out = out_dir().join("timeline.png");
        let page = write_timeline(&json, &production, &out, &StoryboardOptions::default()).expect("timeline");
        assert_eq!(page.cells.len(), 2, "one row per shot");
        assert_eq!(page.cells[1].frames, vec![60, 120], "the second shot maps to frames 60..120");
        assert!(out.exists());
    }

    #[test]
    fn motion_preview_keeps_timing_and_drops_resolution() {
        let (json, production) = fixture();
        let out = out_dir().join("motion-preview.mp4");
        let stats = write_motion_preview(&json, &production, &out, 0.5, &mut |_, _| {}).expect("motion preview");
        assert_eq!((stats.width, stats.height), (160, 90), "half raster keeps the aspect");
        assert_eq!(stats.frames, 120, "every frame of the film is previewed");
        assert!(out.exists());
    }

    #[test]
    fn a_filtered_run_keeps_the_page_number_it_would_have_had() {
        let (json, production) = fixture();
        let dir = out_dir().join("only");
        let options = StoryboardOptions { rows: 1, ..StoryboardOptions::default() };
        let pages = write_storyboard_selected(&json, &production, &dir, &options, &["panel-room".to_owned()]).expect("one page");
        assert_eq!(pages.len(), 1, "only the requested panel's page is written");
        assert_eq!(pages[0].index, 1, "the page keeps the index of the full book");
        assert!(pages[0].path.ends_with("storyboard-002.png"), "{}", pages[0].path);
        assert_eq!(pages[0].cells.len(), 1);
        assert_eq!(pages[0].cells[0].panel, "panel-room");
    }

    #[test]
    fn a_comparison_pair_renders_both_revisions() {
        let (json, production) = fixture();
        let before = json.replacen("#101820", "#203040", 1);
        let out = out_dir().join("comparison.png");
        let page = write_comparison(&before, &json, &production, &out, &[], &StoryboardOptions::default()).expect("comparison");
        assert_eq!(page.panels, vec!["panel-open".to_owned(), "panel-room".to_owned()]);
        let mut library = media_codec::MediaLibrary::new();
        let bitmap = library.image(&page.path, media_codec::DecodeLimits::unbounded()).expect("decode comparison");
        assert_eq!((bitmap.width, bitmap.height), (1600, 1200));
    }

    #[test]
    fn chapters_pair_each_shot_with_its_edges() {
        let (json, production) = fixture();
        let out = out_dir().join("chapters.png");
        let page = write_chapters(&json, &production, &out, &StoryboardOptions::default()).expect("chapters");
        assert_eq!(page.cells.len(), 2);
        assert_eq!(page.cells[0].frames, vec![0, 59], "first and last frame of shot-a");
        assert_eq!(page.cells[1].frames, vec![60, 119], "first and last frame of shot-b");
        assert!(out.exists());
    }

    #[test]
    fn cover_candidates_shortlist_real_frames() {
        let (json, production) = fixture();
        let dir = out_dir().join("covers");
        let candidates = write_cover_candidates(&json, &production, &dir, 3).expect("covers");
        assert_eq!(candidates.len(), 3);
        assert!(candidates.windows(2).all(|pair| pair[0].score >= pair[1].score), "the shortlist is sorted by score");
        for candidate in &candidates {
            assert!(std::path::Path::new(&candidate.path).exists(), "{} is written", candidate.path);
            assert!(candidate.frame < 120, "a candidate is a frame the film shows");
        }
    }

    #[test]
    fn a_scaled_raster_stays_even_for_the_encoder() {
        let (json, production) = fixture();
        let odd = json.replace("\"width\":320", "\"width\":321").replace("\"height\":180", "\"height\":181");
        let out = out_dir().join("motion-preview-odd.mp4");
        let stats = write_motion_preview(&odd, &production, &out, 0.5, &mut |_, _| {}).expect("odd sizes round up");
        assert_eq!(stats.width % 2, 0);
        assert_eq!(stats.height % 2, 0);
        assert_eq!((stats.width, stats.height), (162, 92), "161x91.5 rounds to the next even size");
    }
}
