use core::fmt;
use zeroize::Zeroizing;

/// A secret string handle whose contents never leak through formatting.
///
/// The value is zeroized when the last clone goes out of scope. There is no
/// `Display` implementation and `Debug` prints only a redaction marker, so
/// secrets cannot enter logs, error args, or result envelopes by accident.
#[derive(Clone)]
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    /// Wraps a freshly collected secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// Explicitly reveals the secret for the shortest possible lifetime.
    ///
    /// Callers must not store the returned borrow, print it, or place it
    /// into any error, envelope, or audit record.
    pub fn expose_secret(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(▮)")
    }
}
