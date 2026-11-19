//! `data-binary-ttf` — SFNT font facts for the `data.binary` capability family.
//!
//! This crate parses TrueType and OpenType font files (single `.ttf` / `.otf`
//! or TTC collections) and exposes raw **facts**: table directories, standard
//! metrics, the `MATH` table, TrueType `glyf` outlines and OTF CFF outlines.
//! [`raster::Font`] adds a small, self-contained glyph rasterizer on top of
//! those outlines.
//!
//! It does **not** build SVG paths, select fonts, apply spacing rules or
//! compress/decompress WOFF. Those jobs belong to consumers (notably VuTeX and
//! other formula-layout projects).
//!
//! All parsing is bounded: truncated input, unknown offsets, and bad
//! checksums surface as structured results rather than panics. Unknown tables
//! are preserved in the directory and annotated rather than rejected.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

#![warn(missing_docs)]
#![allow(dead_code)]

pub mod outline;
pub mod raster;
pub mod reader;
pub mod sfnt;
pub mod tables;

// Top-level re-exports for the most common entry points.
pub use outline::{BBox, OutlineBuilder};
pub use reader::{ReadError, Reader, Tag};
pub use sfnt::{
    ChecksumStatus, Face, FontContainer, OffsetTable, ParseError, TableDirectory, TableRecord, file_checksum, magic, parse_font,
    parse_font_validated, table_checksum, validate_checksums,
};

// Diagnostic helpers bridge to `data-binary-core`.
pub mod diag {
    //! Diagnostic constructors prefixed with `binary.ttf.*` wire codes.
    use data_binary_core::{
        diagnostic as bcd,
        diagnostic::{BinaryDiagnosticCode, DiagnosticSeverity},
    };

    /// Stable dotted identifier per ttf-specific diagnostic.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TtfCode {
        /// Table checksum mismatch.
        ChecksumMismatch,
        /// Unknown SFNT version field.
        UnknownVersion,
        /// A required standard table is missing.
        MissingStandardTable,
        /// Out-of-bounds offset inside a table.
        TableOffsetOutOfRange,
        /// Malformed `cmap` subtable.
        CmapMalformed,
        /// Malformed `maxp` table.
        MaxpMalformed,
        /// Unsupported MATH version.
        MathUnsupportedVersion,
        /// MATH substructure truncated.
        MathTruncated,
        /// Glyph ID outside font range.
        GlyphOutOfRange,
    }

    impl TtfCode {
        pub const fn as_str(self) -> &'static str {
            match self {
                Self::ChecksumMismatch => "binary.ttf.checksum",
                Self::UnknownVersion => "binary.ttf.unknown-version",
                Self::MissingStandardTable => "binary.ttf.missing-standard-table",
                Self::TableOffsetOutOfRange => "binary.ttf.offset-out-of-range",
                Self::CmapMalformed => "binary.ttf.cmap-malformed",
                Self::MaxpMalformed => "binary.ttf.maxp-malformed",
                Self::MathUnsupportedVersion => "binary.ttf.math-unsupported-version",
                Self::MathTruncated => "binary.ttf.math-truncated",
                Self::GlyphOutOfRange => "binary.ttf.glyph-out-of-range",
            }
        }
    }

    /// Build a `binary.ttf.*` diagnostic at the given severity.
    pub fn ttf(code: TtfCode, severity: DiagnosticSeverity, message: impl Into<String>) -> bcd::Diagnostic {
        use data_binary_core::diagnostic::*;
        let wire = DiagnosticCode::new(code.as_str());
        let origin = DiagnosticOrigin::new("data-binary-ttf", "table");
        Diagnostic::new(wire, severity, origin, Message::new(code.as_str()).with_fallback(message.into()))
    }

    /// Convenience: wrap a `ReadError` into a `binary.layout.truncated`.
    pub fn bounded(e: crate::reader::ReadError) -> bcd::Diagnostic {
        bcd::error(BinaryDiagnosticCode::Truncated, e.to_string())
    }
}

/// Pull in a minimal license-aware analysis.
pub mod license;

/// Font-level facade that collects parsed tables off a [`Face`].
pub mod font;

// Re-export face-level convenience.
pub use font::FontFace;
pub use raster::{Font as RasterFont, FontError, Metrics};
