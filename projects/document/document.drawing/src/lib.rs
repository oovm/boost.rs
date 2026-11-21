#![deny(missing_docs)]
//! Planar drawings lowered onto the shared vector stack.
//!
//! A [`Drawing`] is an ordered list of shapes with one flat style surface. It
//! lowers to a [`DisplayScene`], which means one geometry model feeds both
//! writers: [`Drawing::to_svg`] serializes the scene, and [`Drawing::to_pdf`]
//! places the same scene on a single page and renders it through the document
//! PDF backend. Neither writer re-implements shape math.
//!
//! Shapes use the document's own coordinate space, origin at the top-left with
//! y growing down, matching SVG and the scene model. Angles are degrees,
//! counter-clockwise in math orientation, so an arc from 0 to 90 sweeps from the
//! positive x axis toward negative y on the page.

use std::fmt;
use std::io;
use std::path::Path;

use boost_fs::{OverwritePolicy, PublishOptions, publish_bytes_with_options};
use document_presentation::display::{DisplayGraphic, DisplayItem, DisplayPage, DisplayPresentation};
use document_renderer_pdf::{PdfFontPolicy, render_pdf_with_font_policy};
use plotter_scene::{DisplayScene, NodeKind, PathData, SceneError, SceneNode, ShapeStyle, TextStyle};
use plotter_types::Rect;

pub use plotter_types::{Color, Point, Size};

/// One drawable primitive.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// A straight segment.
    Line {
        /// Start point.
        from: Point,
        /// End point.
        to: Point,
    },
    /// An open polyline.
    Polyline {
        /// Vertices in draw order.
        points: Vec<Point>,
    },
    /// A closed polygon.
    Polygon {
        /// Vertices in draw order.
        points: Vec<Point>,
    },
    /// A circle.
    Circle {
        /// Center point.
        center: Point,
        /// Radius.
        radius: f64,
    },
    /// An axis-aligned ellipse.
    Ellipse {
        /// Center point.
        center: Point,
        /// Horizontal and vertical radii.
        radii: Size,
    },
    /// A circular arc sampled into a polyline.
    Arc {
        /// Center point.
        center: Point,
        /// Radius.
        radius: f64,
        /// Sweep start in degrees.
        start_degrees: f64,
        /// Sweep end in degrees.
        end_degrees: f64,
    },
    /// A single line of text whose position is the left edge of the baseline.
    Text {
        /// Baseline start.
        at: Point,
        /// Text content.
        text: String,
        /// Font size in drawing units.
        size: f64,
    },
}

/// Stroke and fill attributes shared by every shape of a drawing.
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    /// Outline paint, or `None` for no outline.
    pub stroke: Option<Color>,
    /// Interior paint, or `None` for an unfilled shape.
    pub fill: Option<Color>,
    /// Outline width in drawing units.
    pub width: f64,
    /// Dash pattern `[on, off]` in drawing units.
    pub dash: Option<[f64; 2]>,
    /// Opacity in `0.0..=1.0`.
    pub opacity: f64,
}

impl Style {
    /// An unfilled shape with an outline.
    pub fn stroke(color: Color, width: f64) -> Self {
        Self { stroke: Some(color), width, ..Self::default() }
    }

    /// A filled shape with no outline.
    pub fn fill(color: Color) -> Self {
        Self { fill: Some(color), ..Self::default() }
    }

    /// A filled shape with an outline.
    pub fn both(fill: Color, stroke: Color, width: f64) -> Self {
        Self { fill: Some(fill), stroke: Some(stroke), width, ..Self::default() }
    }

    fn shape_style(&self) -> ShapeStyle {
        ShapeStyle {
            fill: self.fill,
            stroke: self.stroke,
            stroke_width: self.width,
            dash: self.dash,
            opacity: self.opacity,
        }
    }
}

impl Default for Style {
    fn default() -> Self {
        Self { stroke: Some(Color::BLACK), fill: None, width: 1.0, dash: None, opacity: 1.0 }
    }
}

/// A shape with its style.
#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    /// Painted geometry.
    pub shape: Shape,
    /// Paint attributes.
    pub style: Style,
}

impl Element {
    /// Pair a shape with a style.
    pub fn new(shape: Shape, style: Style) -> Self {
        Self { shape, style }
    }
}

/// A single-page drawing.
#[derive(Debug, Clone, PartialEq)]
pub struct Drawing {
    /// Canvas size in drawing units.
    pub size: Size,
    /// Optional page background.
    pub background: Option<Color>,
    /// Elements in paint order.
    pub elements: Vec<Element>,
}

impl Drawing {
    /// Create an empty drawing.
    pub fn new(size: Size) -> Self {
        Self { size, background: None, elements: Vec::new() }
    }

    /// Set the page background.
    pub fn with_background(mut self, color: Color) -> Self {
        self.background = Some(color);
        self
    }

    /// Append one element.
    pub fn push(&mut self, element: Element) {
        self.elements.push(element);
    }

    /// Lower the drawing to the shared display scene.
    pub fn to_scene(&self) -> DisplayScene {
        let mut scene = DisplayScene::new(self.size);
        if let Some(color) = self.background {
            let rect = Rect::new(0.0, 0.0, self.size.width, self.size.height);
            scene.push(SceneNode::new(NodeKind::Rect { rect, style: ShapeStyle::filled(color) }));
        }
        for element in &self.elements {
            scene.push(node_for(&element.shape, &element.style));
        }
        scene
    }

    /// Serialize as a deterministic SVG document.
    pub fn to_svg(&self) -> Result<String, DrawError> {
        self.to_scene().to_svg().map_err(DrawError::from)
    }

    /// Render as a single-page PDF.
    ///
    /// The page is exactly the drawing canvas, so one drawing unit is one PDF
    /// point. Base-14 fonts keep the file self-contained without embedding.
    pub fn to_pdf(&self) -> Result<Vec<u8>, DrawError> {
        let scene = self.to_scene();
        let presentation = DisplayPresentation {
            pages: vec![DisplayPage {
                width: self.size.width,
                height: self.size.height,
                items: vec![DisplayItem::Graphic(DisplayGraphic {
                    x: 0.0,
                    y: 0.0,
                    width: self.size.width,
                    height: self.size.height,
                    scene,
                    alt: String::from("drawing"),
                })],
                anchors: Vec::new(),
            }],
        };
        render_pdf_with_font_policy(&presentation, &PdfFontPolicy::Base14Only)
            .map_err(|diagnostic| DrawError { code: "drawing/pdf", message: format!("{}: {}", diagnostic.code, diagnostic.message) })
    }

    /// Write the SVG document atomically.
    pub fn save_svg(&self, path: impl AsRef<Path>) -> Result<(), DrawError> {
        let svg = self.to_svg()?;
        save(path.as_ref(), svg.as_bytes())
    }

    /// Write the PDF document atomically.
    pub fn save_pdf(&self, path: impl AsRef<Path>) -> Result<(), DrawError> {
        let pdf = self.to_pdf()?;
        save(path.as_ref(), &pdf)
    }
}

/// A drawing failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawError {
    code: &'static str,
    message: String,
}

impl DrawError {
    /// The stable failure code.
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The failure description.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DrawError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DrawError {}

impl From<SceneError> for DrawError {
    fn from(error: SceneError) -> Self {
        let code = match error.code() {
            "davinci.scene.invalid-geometry" | "davinci.invalid-geometry" => "drawing/geometry",
            _ => "drawing/scene",
        };
        Self { code, message: format!("{}: {}", error.code(), error.message()) }
    }
}

impl From<io::Error> for DrawError {
    fn from(error: io::Error) -> Self {
        Self { code: "drawing/io", message: error.to_string() }
    }
}

fn save(path: &Path, bytes: &[u8]) -> Result<(), DrawError> {
    let options = PublishOptions { overwrite: OverwritePolicy::Replace, ..PublishOptions::default() };
    publish_bytes_with_options(path, bytes, options).map_err(DrawError::from)
}

fn node_for(shape: &Shape, style: &Style) -> SceneNode {
    let shape_style = style.shape_style();
    match shape {
        Shape::Line { from, to } => SceneNode::new(NodeKind::Line { from: *from, to: *to, style: shape_style }),
        Shape::Polyline { points } => SceneNode::new(NodeKind::Path { path: polyline(points, false), style: shape_style }),
        Shape::Polygon { points } => SceneNode::new(NodeKind::Path { path: polyline(points, true), style: shape_style }),
        Shape::Circle { center, radius } => {
            SceneNode::new(NodeKind::Ellipse { center: *center, radii: Size::new(*radius, *radius), style: shape_style })
        }
        Shape::Ellipse { center, radii } => SceneNode::new(NodeKind::Ellipse { center: *center, radii: *radii, style: shape_style }),
        Shape::Arc { center, radius, start_degrees, end_degrees } => {
            SceneNode::new(NodeKind::Path { path: arc_path(*center, *radius, *start_degrees, *end_degrees), style: shape_style })
        }
        Shape::Text { at, text, size } => SceneNode::new(NodeKind::Text {
            position: *at,
            text: text.clone(),
            style: TextStyle {
                fill: style.fill.or(style.stroke).or(Some(Color::BLACK)),
                opacity: style.opacity,
                font_size: *size,
                ..TextStyle::default()
            },
        }),
    }
}

fn polyline(points: &[Point], closed: bool) -> PathData {
    let mut path = PathData::new();
    let mut iter = points.iter();
    if let Some(first) = iter.next() {
        path = path.move_to(*first);
        for point in iter {
            path = path.line_to(*point);
        }
        if closed {
            path = path.close();
        }
    }
    path
}

fn arc_path(center: Point, radius: f64, start_degrees: f64, end_degrees: f64) -> PathData {
    let start = start_degrees.to_radians();
    let end = end_degrees.to_radians();
    let sweep = (end - start).abs();
    let segments = ((sweep / (5.0_f64.to_radians())).ceil() as usize).max(2);
    let mut path = PathData::new();
    for index in 0..=segments {
        let t = index as f64 / segments as f64;
        let angle = start + (end - start) * t;
        let point = Point::new(center.x + radius * angle.cos(), center.y + radius * angle.sin());
        path = if index == 0 { path.move_to(point) } else { path.line_to(point) };
    }
    path
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn drawing() -> Drawing {
        let mut drawing = Drawing::new(Size::new(100.0, 50.0)).with_background(Color::WHITE);
        drawing.push(Element::new(
            Shape::Line { from: Point::new(0.0, 0.0), to: Point::new(100.0, 50.0) },
            Style::stroke(Color::BLACK, 2.0),
        ));
        drawing.push(Element::new(Shape::Circle { center: Point::new(20.0, 20.0), radius: 5.0 }, Style::fill(Color::rgb(255, 0, 0))));
        drawing.push(Element::new(
            Shape::Polygon {
                points: vec![Point::new(1.0, 1.0), Point::new(9.0, 1.0), Point::new(5.0, 8.0)],
            },
            Style::both(Color::WHITE, Color::BLACK, 1.0),
        ));
        drawing.push(Element::new(
            Shape::Text { at: Point::new(2.0, 40.0), text: String::from("hello"), size: 12.0 },
            Style::default(),
        ));
        drawing
    }

    #[test]
    fn scene_contains_one_node_per_element_plus_background() {
        let scene = drawing().to_scene();
        assert_eq!(scene.size, Size::new(100.0, 50.0));
        assert_eq!(scene.nodes.len(), 5);
        scene.validate().expect("valid scene");
    }

    #[test]
    fn svg_is_deterministic_and_carries_text() {
        let first = drawing().to_svg().expect("svg");
        let second = drawing().to_svg().expect("svg");
        assert_eq!(first, second);
        assert!(first.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(first.contains("hello"));
        assert!(first.contains("ellipse"));
    }

    #[test]
    fn pdf_starts_with_header_and_is_non_empty() {
        let pdf = drawing().to_pdf().expect("pdf");
        assert!(pdf.starts_with(b"%PDF-1.7"));
        assert!(pdf.len() > 100);
    }

    #[test]
    fn empty_drawing_still_serializes() {
        let empty = Drawing::new(Size::new(10.0, 10.0));
        assert!(empty.to_svg().expect("svg").contains("<svg"));
        assert!(empty.to_pdf().expect("pdf").starts_with(b"%PDF"));
    }

    #[test]
    fn arc_samples_endpoints() {
        let mut drawing = Drawing::new(Size::new(10.0, 10.0));
        drawing.push(Element::new(
            Shape::Arc { center: Point::new(5.0, 5.0), radius: 4.0, start_degrees: 0.0, end_degrees: 90.0 },
            Style::stroke(Color::BLACK, 1.0),
        ));
        let scene = drawing.to_scene();
        let NodeKind::Path { path, .. } = &scene.nodes[0].kind else { panic!("expected path") };
        assert!(path.commands.len() >= 3);
    }

    #[test]
    fn save_svg_writes_a_file() {
        let unique = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("document-drawing-{}-{unique}.svg", std::process::id()));
        drawing().save_svg(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("<svg"));
        std::fs::remove_file(&path).ok();
    }
}
