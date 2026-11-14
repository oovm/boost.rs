/// Failure raised while turning shader text into UCF IR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontendError {
    /// The shader parser rejected the source.
    Parse(String),
    /// The construct is valid shader syntax that this front-end does not lower yet.
    Unsupported(String),
    /// The lowered program failed UCF program validation.
    Invalid(String),
}

impl std::fmt::Display for FrontendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(message) => write!(f, "shader parse failed: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported shader construct: {message}"),
            Self::Invalid(message) => write!(f, "lowered program is invalid: {message}"),
        }
    }
}

impl std::error::Error for FrontendError {}
