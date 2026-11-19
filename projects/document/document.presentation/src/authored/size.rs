//! Deck size in English Metric Units.
//!
//! The authored model stores EMU, the unit the PPTX reader and writer already
//! use. The authoring surface accepts points and converts once at the boundary;
//! screen pixels never enter the model.

use serde::{Deserialize, Serialize};

/// English Metric Units per typographic point.
pub const EMU_PER_POINT: i64 = 12_700;

/// Slide extent in EMU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckSize {
    /// Slide width in EMU.
    pub width_emu: i64,
    /// Slide height in EMU.
    pub height_emu: i64,
}

impl DeckSize {
    /// `16:9` at the PowerPoint widescreen extent of 13.333 × 7.5 inches.
    pub const WIDESCREEN: Self = Self { width_emu: 12_192_000, height_emu: 6_858_000 };
    /// `4:3` at the classic extent of 10 × 7.5 inches.
    pub const STANDARD: Self = Self { width_emu: 9_144_000, height_emu: 6_858_000 };
    /// `16:10` at 13.333 × 8.333 inches.
    pub const SIXTEEN_TEN: Self = Self { width_emu: 12_192_000, height_emu: 7_620_000 };

    /// Resolves a named authoring ratio such as `16:9`.
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "16:9" => Some(Self::WIDESCREEN),
            "4:3" => Some(Self::STANDARD),
            "16:10" => Some(Self::SIXTEEN_TEN),
            _ => None,
        }
    }

    /// Builds a custom extent from point values.
    pub fn from_points(width_pt: f64, height_pt: f64) -> Option<Self> {
        let width_emu = points_to_emu(width_pt)?;
        let height_emu = points_to_emu(height_pt)?;
        (width_emu > 0 && height_emu > 0).then_some(Self { width_emu, height_emu })
    }

    /// Slide width in points, for the authoring surface.
    pub fn width_pt(&self) -> f64 {
        emu_to_points(self.width_emu)
    }

    /// Slide height in points, for the authoring surface.
    pub fn height_pt(&self) -> f64 {
        emu_to_points(self.height_emu)
    }
}

/// Converts points to EMU, rejecting non-finite and out-of-range values.
pub fn points_to_emu(points: f64) -> Option<i64> {
    if !points.is_finite() || points <= 0.0 {
        return None;
    }
    let emu = (points * EMU_PER_POINT as f64).round();
    (emu <= i64::MAX as f64).then_some(emu as i64)
}

/// Converts EMU to points, the unit of the authoring surface.
pub fn emu_to_points(emu: i64) -> f64 {
    emu as f64 / EMU_PER_POINT as f64
}
