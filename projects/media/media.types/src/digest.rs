//! Content digests for assets and other bytes a work depends on.
//!
//! One algorithm lives here so a production book, a render report and an
//! import step all quote the same value for the same bytes. A digest is
//! provenance, not a security primitive: it detects "this file changed", it
//! does not authenticate a publisher.

/// FNV-1a over `bytes`, rendered as lowercase hex.
///
/// The value matches what every render report already quotes for fonts and
/// imported assets, so a stored `Asset.digest` can be compared with a fresh
/// render without translation.
#[must_use]
pub fn digest(bytes: &[u8]) -> String {
    let mut state = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{state:016x}")
}

#[cfg(test)]
mod tests {
    use super::digest;

    /// Fixed vectors keep the format stable: a stored digest must still
    /// compare after the algorithm is touched.
    #[test]
    fn known_values_are_stable() {
        assert_eq!(digest(b""), "cbf29ce484222325");
        assert_eq!(digest(b"a"), "af63dc4c8601ec8c");
        assert_eq!(digest(b"foobar"), "85944171f73967e8");
    }

    #[test]
    fn one_changed_byte_changes_the_digest() {
        assert_ne!(digest(b"frame-001"), digest(b"frame-002"));
    }
}
