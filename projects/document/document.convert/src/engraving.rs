//! Host-side score rendering through the `plotter.engraving` chain.
//!
//! `notedown-formats` promotes `abc` and `musicxml` fences to `Block::Engraving`.
//! This module lowers those blocks with the Oak ABC frontend
//! (`plotter.input.abc`) plus `plotter.engraving`, and serializes each resolved
//! display scene to SVG, keyed by the authoring block id, so the HTML writer can
//! embed real vector output instead of the source-only fallback.
//!
//! `musicxml` still has no frontend in this build. Those blocks keep the visible
//! source and record `davinci.engraving.ast-unavailable` instead of inventing a
//! score. A block that fails to lower or serialize keeps no scene and records
//! the stable `davinci.*` code as a loss marker.

use std::collections::HashMap;

use document_formats::export::html::RenderedDiagram;
use document_ir::{Block, DocumentGraph, LossMarker, NodeId, SemanticStatus, SourceSpan};
use plotter_input_abc::abc_to_scene;
use plotter_types::codes;

use crate::span::absolute_span;

/// Score scenes rendered for the HTML writer plus the losses they caused.
#[derive(Default)]
pub struct RenderedEngravings {
    /// Inline SVG keyed by the authoring score block.
    pub scenes: HashMap<NodeId, RenderedDiagram>,
    /// Loss markers for scores whose scene is missing or approximate.
    pub losses: Vec<LossMarker>,
    /// Authored document spans aligned with `losses`.
    pub loss_spans: Vec<Option<SourceSpan>>,
}

impl RenderedEngravings {
    fn push_loss(&mut self, marker: LossMarker, span: Option<SourceSpan>) {
        self.losses.push(marker);
        self.loss_spans.push(span);
    }
}

/// Render every `Block::Engraving` in `graph` to an SVG scene.
///
/// Non-fatal lowering diagnostics (dropped decorations, approximated bar lines,
/// implicit rests) keep the rendered scene but record `Partial` loss. Fatal
/// failures record the stable `davinci.engraving.*` code as `Unsupported` loss
/// and the block keeps the visible source fallback.
pub fn render_engravings(graph: &DocumentGraph) -> RenderedEngravings {
    let mut rendered = RenderedEngravings::default();
    for node in &graph.blocks {
        let Block::Engraving { language, source } = &node.block
        else {
            continue;
        };
        let language = language.trim();
        if !is_engraving_language(language) {
            continue;
        }
        let block_span = graph.block_span(node.id);
        if !language.eq_ignore_ascii_case("abc") {
            rendered.push_loss(
                LossMarker {
                    code: codes::ENGRAVING_AST_UNAVAILABLE.to_string(),
                    message: format!(
                        "score language `{language}` has no frontend in this build: the text grammar lands in Oak \
                         (`oak-abc`), so the authored fence stays visible"
                    ),
                    status: SemanticStatus::Unsupported,
                },
                absolute_span(block_span, None),
            );
            continue;
        }
        match abc_to_scene(source) {
            Ok(lowered) => {
                for diagnostic in &lowered.diagnostics {
                    rendered.push_loss(
                        LossMarker {
                            code: diagnostic.code().as_str().to_string(),
                            message: diagnostic.message().to_string(),
                            status: SemanticStatus::Partial,
                        },
                        block_span,
                    );
                }
                let size = lowered.scene.size;
                if size.width <= 0.0 || size.height <= 0.0 {
                    rendered.push_loss(
                        LossMarker {
                            code: "panduck.engraving.invalid-canvas".to_string(),
                            message: format!("Engraving lowered to a non-positive canvas: {size:?}"),
                            status: SemanticStatus::Lossy,
                        },
                        block_span,
                    );
                    continue;
                }
                match lowered.scene.to_svg_with_authored_hooks(&format!("engraving-{}", node.id.0), "engraving", &plotter_types::DIAGRAM_THEME)
                {
                    Ok(svg) => {
                        rendered.scenes.insert(node.id, RenderedDiagram { svg });
                    }
                    Err(error) => {
                        rendered.push_loss(
                            LossMarker { code: error.code().to_string(), message: error.message().to_string(), status: SemanticStatus::Lossy },
                            block_span,
                        );
                    }
                }
            }
            Err(error) => {
                rendered.push_loss(
                    LossMarker { code: error.code().to_string(), message: error.message().to_string(), status: SemanticStatus::Unsupported },
                    absolute_span(block_span, error.span()),
                );
            }
        }
    }
    rendered
}

/// Score languages registered by the format layer.
fn is_engraving_language(language: &str) -> bool {
    language.eq_ignore_ascii_case("abc") || language.eq_ignore_ascii_case("musicxml")
}
