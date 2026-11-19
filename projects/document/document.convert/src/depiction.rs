//! Host-side chemical structure rendering through the `plotter.depiction` chain.
//!
//! `notedown-formats` promotes `smiles`, `mol`, `mdl`, `sdf`, and `rd` fences to
//! `Block::Depiction`. This module lowers those blocks with the SMILES and
//! MDL frontends (`plotter.input.smiles` / `plotter.input.mdl`) plus
//! `plotter.depiction`, and serializes each resolved display scene to SVG,
//! keyed by the authoring block id, so the HTML writer can embed real vector
//! output instead of the source-only fallback.
//!
//! A block that fails to lower or serialize keeps no scene and records the
//! stable `davinci.depiction.*` code as a loss marker.

use std::collections::HashMap;

use document_formats::export::html::RenderedDiagram;
use document_ir::{Block, DocumentGraph, LossMarker, NodeId, SemanticStatus, SourceSpan};
use plotter_input_mdl::mdl_to_scene;
use plotter_input_smiles::smiles_to_scene;
use plotter_types::codes;

use crate::span::absolute_span;

/// Structure scenes rendered for the HTML writer plus the losses they caused.
#[derive(Default)]
pub struct RenderedDepictions {
    /// Inline SVG keyed by the authoring structure block.
    pub scenes: HashMap<NodeId, RenderedDiagram>,
    /// Loss markers for structures whose scene is missing or approximate.
    pub losses: Vec<LossMarker>,
    /// Authored document spans aligned with `losses`.
    pub loss_spans: Vec<Option<SourceSpan>>,
}

impl RenderedDepictions {
    fn push_loss(&mut self, marker: LossMarker, span: Option<SourceSpan>) {
        self.losses.push(marker);
        self.loss_spans.push(span);
    }
}

/// Render every `Block::Depiction` in `graph` to an SVG scene.
///
/// Non-fatal lowering diagnostics (flattened stereo, replaced coordinates,
/// crowded placement) keep the rendered scene but record `Partial` loss. Fatal
/// failures record the stable `davinci.depiction.*` code as `Unsupported` loss
/// and the block keeps the visible source fallback.
pub fn render_depictions(graph: &DocumentGraph) -> RenderedDepictions {
    let mut rendered = RenderedDepictions::default();
    for node in &graph.blocks {
        let Block::Depiction { language, source } = &node.block
        else {
            continue;
        };
        let language = language.trim();
        if !is_depiction_language(language) {
            continue;
        }
        let block_span = graph.block_span(node.id);
        let lowered = if language.eq_ignore_ascii_case("smiles") {
            smiles_to_scene(source)
        }
        else if language.eq_ignore_ascii_case("mol") || language.eq_ignore_ascii_case("mdl") || language.eq_ignore_ascii_case("sdf") {
            mdl_to_scene(source)
        }
        else {
            // `rd` is registered by the format layer but has no frontend yet.
            rendered.push_loss(
                LossMarker {
                    code: codes::DEPICTION_AST_UNAVAILABLE.to_string(),
                    message: format!(
                        "structure language `{language}` has no frontend in this build: the text grammar lands in \
                         Oak (`oak-smiles` / `oak-mdl`), so the authored fence stays visible"
                    ),
                    status: SemanticStatus::Unsupported,
                },
                absolute_span(block_span, None),
            );
            continue;
        };
        match lowered {
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
                            code: "panduck.depiction.invalid-canvas".to_string(),
                            message: format!("Depiction lowered to a non-positive canvas: {size:?}"),
                            status: SemanticStatus::Lossy,
                        },
                        block_span,
                    );
                    continue;
                }
                match lowered.scene.to_svg_with_authored_hooks(&format!("depiction-{}", node.id.0), "depiction", &plotter_types::DIAGRAM_THEME)
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

/// Structure languages registered by the format layer.
fn is_depiction_language(language: &str) -> bool {
    language.eq_ignore_ascii_case("smiles")
        || language.eq_ignore_ascii_case("mol")
        || language.eq_ignore_ascii_case("mdl")
        || language.eq_ignore_ascii_case("sdf")
        || language.eq_ignore_ascii_case("rd")
}
