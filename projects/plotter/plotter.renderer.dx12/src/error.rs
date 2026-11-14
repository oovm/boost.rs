//! Structured failures produced while turning a scene into pixels.

/// A scene that cannot be converted into a triangle list or rasterised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// The raster target must have a non-zero width and height.
    EmptyTarget {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
    },
    /// The scene graph references a node that is absent from the arena.
    MissingNode {
        /// Dangling node id.
        node: u64,
    },
    /// The node kind has no triangle conversion in this backend.
    UnsupportedNode {
        /// Node kind name.
        kind: &'static str,
        /// Node id owning the unsupported payload.
        node: u64,
    },
    /// A mesh index points outside its position array.
    InvalidIndex {
        /// Mesh node id.
        node: u64,
        /// Offending index.
        index: u32,
    },
    /// Direct3D 12 device creation or draw failure.
    Backend(String),
}

impl RenderError {
    /// Build an [`RenderError::UnsupportedNode`].
    pub(crate) const fn unsupported(kind: &'static str, node: u64) -> Self {
        Self::UnsupportedNode { kind, node }
    }

    /// Build an [`RenderError::Backend`] from any displayable error.
    pub(crate) fn backend(err: impl core::fmt::Display) -> Self {
        Self::Backend(err.to_string())
    }
}

impl core::fmt::Display for RenderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyTarget { width, height } => {
                write!(f, "raster target must be non-zero, got {width}x{height}")
            }
            Self::MissingNode { node } => write!(f, "scene node {node} is missing from the arena"),
            Self::UnsupportedNode { kind, node } => {
                write!(f, "node {node} ({kind}) has no Direct3D 12 triangle conversion")
            }
            Self::InvalidIndex { node, index } => {
                write!(f, "mesh node {node} references vertex {index} outside its position array")
            }
            Self::Backend(message) => write!(f, "Direct3D 12 backend failed: {message}"),
        }
    }
}

impl std::error::Error for RenderError {}
