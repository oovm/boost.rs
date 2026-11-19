//! Rule checks over one committed revision.
//!
//! These checks answer geometric and rule questions only. Aesthetic judgement,
//! narrative pacing and information design still require a real visual review;
//! no combined "quality score" is produced here.
//!
//! The static phase implements out-of-bounds, missing-asset and a basic contrast
//! check. Checks that need text measurement (overflow, alignment) or font
//! resolution report themselves as unsupported instead of passing silently.

use super::{
    error::codes,
    id::{ElementId, SlideId},
    model::{Deck, ElementContent, Fill},
};

/// Version of the checker that produced a report.
pub const CHECKER_VERSION: &str = "authored-p1";

/// One requested rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectCheck {
    /// Text does not fit its box; needs text measurement, not available yet.
    Overflow,
    /// Text and background are too close in luminance.
    Contrast,
    /// Elements that look aligned are not; not available yet.
    Alignment,
    /// Referenced assets exist and carry bytes.
    Assets,
    /// Requested fonts resolve; not available yet.
    Fonts,
    /// Elements lie fully inside the slide.
    OutOfBounds,
}

impl InspectCheck {
    /// Stable kebab-case name used by the authoring surface.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overflow => "overflow",
            Self::Contrast => "contrast",
            Self::Alignment => "alignment",
            Self::Assets => "assets",
            Self::Fonts => "fonts",
            Self::OutOfBounds => "out-of-bounds",
        }
    }

    /// Parses the authoring name, returning `None` for unknown checks.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "overflow" => Some(Self::Overflow),
            "contrast" => Some(Self::Contrast),
            "alignment" => Some(Self::Alignment),
            "assets" => Some(Self::Assets),
            "fonts" => Some(Self::Fonts),
            "out-of-bounds" => Some(Self::OutOfBounds),
            _ => None,
        }
    }
}

/// Severity of an inspection finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Informational, such as a check that is not implemented yet.
    Info,
    /// The deck is still usable but the finding should be reviewed.
    Warning,
    /// The deck breaks a rule that must be fixed before delivery.
    Error,
}

/// One finding of an inspection run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectIssue {
    /// Stable diagnostic code.
    pub code: String,
    /// Severity.
    pub severity: Severity,
    /// Revision the finding belongs to.
    pub revision: u64,
    /// Slide the finding belongs to.
    pub slide_id: Option<SlideId>,
    /// Element the finding belongs to.
    pub element_id: Option<ElementId>,
    /// Whether the caller can act on the finding without rebuilding the deck.
    pub recoverable: bool,
    /// Human-readable description.
    pub message: String,
}

/// Result of one inspection run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectReport {
    /// Revision that was inspected.
    pub revision: u64,
    /// Checker version.
    pub checker_version: String,
    /// Findings in slide order.
    pub issues: Vec<InspectIssue>,
    /// Whether no error-level finding was reported.
    pub passed: bool,
}

/// Options of [`Deck::inspect`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectOptions {
    /// Rules to run; all implemented rules run when absent.
    pub checks: Option<Vec<InspectCheck>>,
}

impl Deck {
    /// Runs rule checks against the current revision.
    pub fn inspect(&self, options: InspectOptions) -> InspectReport {
        let checks = options.checks.unwrap_or_else(|| vec![InspectCheck::OutOfBounds, InspectCheck::Contrast, InspectCheck::Assets]);
        let mut issues = Vec::new();
        for check in &checks {
            match check {
                InspectCheck::OutOfBounds => self.check_out_of_bounds(&mut issues),
                InspectCheck::Assets => self.check_assets(&mut issues),
                InspectCheck::Contrast => self.check_contrast(&mut issues),
                InspectCheck::Overflow | InspectCheck::Alignment | InspectCheck::Fonts => {
                    issues.push(self.issue(
                        "deck.check-unsupported",
                        Severity::Info,
                        None,
                        None,
                        format!("the `{}` check is not implemented in this phase", check.as_str()),
                    ));
                }
            }
        }
        let passed = !issues.iter().any(|issue| issue.severity == Severity::Error);
        InspectReport { revision: self.revision(), checker_version: CHECKER_VERSION.to_string(), issues, passed }
    }

    fn check_out_of_bounds(&self, issues: &mut Vec<InspectIssue>) {
        let size = self.size();
        for slide in self.slides() {
            for element in &slide.elements {
                if !element.transform.inside(size) {
                    issues.push(self.issue(
                        "deck.out-of-bounds",
                        Severity::Warning,
                        Some(slide.id.clone()),
                        Some(element.id.clone()),
                        format!("element extends outside the {:.2} × {:.2} pt slide", size.width_pt(), size.height_pt()),
                    ));
                }
            }
        }
    }

    fn check_assets(&self, issues: &mut Vec<InspectIssue>) {
        for slide in self.slides() {
            for element in &slide.elements {
                let asset = match &element.content {
                    ElementContent::Image(image) => Some(&image.asset),
                    ElementContent::Media(media) => Some(&media.asset),
                    _ => None,
                };
                if let Some(asset) = asset
                    && self.asset(asset).is_none()
                {
                    issues.push(self.issue(
                        codes::ASSET_MISSING,
                        Severity::Error,
                        Some(slide.id.clone()),
                        Some(element.id.clone()),
                        format!("element references asset `{asset}`, which is not embedded"),
                    ));
                }
            }
        }
    }

    fn check_contrast(&self, issues: &mut Vec<InspectIssue>) {
        for slide in self.slides() {
            let slide_fill = slide.background.as_ref();
            for element in &slide.elements {
                let ElementContent::Text(text) = &element.content
                else {
                    continue;
                };
                let Some(text_color) =
                    text.paragraphs.iter().flat_map(|paragraph| &paragraph.runs).find_map(|run| run.color.as_deref().and_then(parse_hex_color))
                else {
                    continue;
                };
                let background = element.style.fill.as_ref().or(slide_fill).and_then(solid_color);
                let Some(background) = background.and_then(parse_hex_color)
                else {
                    continue;
                };
                let ratio = contrast_ratio(text_color, background);
                if ratio < 4.5 {
                    issues.push(self.issue(
                        "deck.contrast",
                        Severity::Warning,
                        Some(slide.id.clone()),
                        Some(element.id.clone()),
                        format!("text contrast ratio {ratio:.2}:1 is below the 4.5:1 reading threshold"),
                    ));
                }
            }
        }
    }

    fn issue(&self, code: &str, severity: Severity, slide_id: Option<SlideId>, element_id: Option<ElementId>, message: String) -> InspectIssue {
        InspectIssue {
            code: code.to_string(),
            severity,
            revision: self.revision(),
            slide_id,
            element_id,
            recoverable: severity != Severity::Error,
            message,
        }
    }
}

fn solid_color(fill: &Fill) -> Option<&str> {
    match fill {
        Fill::Solid { color } => Some(color),
        Fill::None | Fill::Inherit => None,
    }
}

/// Parses `#rrggbb` or `#rgb` into linearised sRGB channels.
fn parse_hex_color(value: &str) -> Option<[f64; 3]> {
    let hex = value.strip_prefix('#')?;
    let expand = |pair: &str| u8::from_str_radix(pair, 16).ok();
    let (r, g, b) = match hex.len() {
        3 => {
            let mut chars = hex.chars();
            let r = expand(&chars.next()?.to_string().repeat(2))?;
            let g = expand(&chars.next()?.to_string().repeat(2))?;
            let b = expand(&chars.next()?.to_string().repeat(2))?;
            (r, g, b)
        }
        6 => (expand(&hex[0..2])?, expand(&hex[2..4])?, expand(&hex[4..6])?),
        _ => return None,
    };
    Some([linearise(f64::from(r) / 255.0), linearise(f64::from(g) / 255.0), linearise(f64::from(b) / 255.0)])
}

fn linearise(channel: f64) -> f64 {
    if channel <= 0.04045 { channel / 12.92 } else { ((channel + 0.055) / 1.055).powf(2.4) }
}

fn relative_luminance(color: [f64; 3]) -> f64 {
    0.2126 * color[0] + 0.7152 * color[1] + 0.0722 * color[2]
}

fn contrast_ratio(left: [f64; 3], right: [f64; 3]) -> f64 {
    let left = relative_luminance(left);
    let right = relative_luminance(right);
    let (lighter, darker) = if left >= right { (left, right) } else { (right, left) };
    (lighter + 0.05) / (darker + 0.05)
}
