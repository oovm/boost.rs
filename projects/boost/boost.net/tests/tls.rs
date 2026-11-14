//! TLS protocol version selection.

use boost_net::tls::{TlsConfig, TlsVersion};

#[test]
fn default_offers_tls12_and_tls13() {
    assert_eq!(TlsConfig::default().versions(), [TlsVersion::Tls12, TlsVersion::Tls13]);
}

#[test]
fn only_keeps_one_version() {
    let config = TlsConfig::only(TlsVersion::Tls13);
    assert_eq!(config.versions(), [TlsVersion::Tls13]);
    assert!(config.contains(TlsVersion::Tls13));
    assert!(!config.contains(TlsVersion::Tls12));
}

#[test]
fn new_preserves_order_and_drops_duplicates() {
    let config = TlsConfig::new([TlsVersion::Tls13, TlsVersion::Tls12, TlsVersion::Tls13]).unwrap();
    assert_eq!(config.versions(), [TlsVersion::Tls13, TlsVersion::Tls12]);
}

#[test]
fn empty_selection_is_rejected() {
    assert!(TlsConfig::new([]).is_err());
}

#[test]
fn version_labels_are_stable() {
    assert_eq!(TlsVersion::Tls12.as_str(), "TLS1.2");
    assert_eq!(TlsVersion::Tls13.as_str(), "TLS1.3");
    assert_eq!(TlsVersion::Tls13.to_string(), "TLS1.3");
}
