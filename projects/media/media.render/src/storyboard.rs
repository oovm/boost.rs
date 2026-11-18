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

use crate::render::{render_frame_at, resolve_fonts};

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
    production.validate()?;
    let project = parse_project(json)?;
    let mut fonts = fonts_for(&project)?;
    let blocks = plan_blocks(&project, production)?;
    if blocks.is_empty() {
        return Err(Error::invalid("the production declares no storyboard panel"));
    }
    std::fs::create_dir_all(out_dir).map_err(|e| Error::io(format!("cannot create {}: {e}", out_dir.display())))?;

    let per_page = options.rows.max(1) as usize;
    let mut pages = Vec::new();
    for (page_index, chunk) in blocks.chunks(per_page).enumerate() {
        let path = out_dir.join(format!("storyboard-{:03}.png", page_index + 1));
        let mut page = Frame::new(options.width, options.height);
        page.fill(rgba(options.background, 255));
        draw_title(&mut fonts, &mut page, options, &format!("Storyboard {} / {}", page_index + 1, blocks.len().div_ceil(per_page)));

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
    let caption_height = options.label_size * 3.6;
    let thumb_height = (row_height - caption_height).max(options.label_size * 3.0);
    // One panel row always carries exactly three key frames.
    let columns = 3;
    let thumb_width = (content_width - options.gap * (columns - 1) as f32) / columns as f32;

    for (index, frame_index) in block.frames.iter().enumerate() {
        let x = options.margin + (thumb_width + options.gap) * index as f32;
        let frame = render_frame_at(json, *frame_index)?;
        page.round_rect(x, top, thumb_width, thumb_height, 4.0, rgba([18, 22, 30], 255));
        page.draw_bitmap(&bitmap(frame), x, top, thumb_width, thumb_height, Fit::Contain, 1.0, 4.0);
        let tag = match index {
            0 => "first",
            1 => "middle",
            _ => "last",
        };
        let caption = format!("{tag} · f{}", frame_index);
        draw_text(fonts, page, options, &caption, x, top + thumb_height + options.label_size, options.label_size * 0.85, thumb_width);
    }

    let baseline = top + thumb_height + options.label_size * 2.1;
    let header = format!("{} · {} · frames {}..{}", block.shot, block.panel, block.frames[0], block.frames[2] + 1);
    draw_text(fonts, page, options, &header, options.margin, baseline, options.label_size * 1.15, content_width);
    let detail = match &block.dialogue {
        Some(line) => format!("{} / {} / \u{201c}{line}\u{201d}", block.purpose, block.composition),
        None => format!("{} / {}", block.purpose, block.composition),
    };
    draw_text(fonts, page, options, &detail, options.margin, baseline + options.label_size * 1.5, options.label_size, content_width);
    Ok(())
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
}
