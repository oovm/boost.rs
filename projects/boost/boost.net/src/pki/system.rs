//! Roots installed in the operating system trust store.
//!
//! [`super::lets_encrypt_anchors`] trust only Let's Encrypt: enough for the
//! family's own services, far too narrow for every public host a desktop app
//! talks to (`api.trae.cn` chains to DigiCert, for example). Widening the set
//! to what the platform already trusts keeps a normal OS deployment working
//! without pinning each public CA by hand.
//!
//! The store is read once per process by the caller; nothing here caches.

use alloc::vec::Vec;

use super::TrustAnchor;

/// Trust anchors read from the platform certificate store.
///
/// Returns an empty list on platforms whose store is not readable from here,
/// which leaves the caller with the pinned anchors only.
#[cfg(all(feature = "std", windows))]
pub fn system_anchors() -> Vec<TrustAnchor> {
    windows_store::anchors()
}

/// Trust anchors read from the platform certificate store.
///
/// Returns an empty list on platforms whose store is not readable from here,
/// which leaves the caller with the pinned anchors only.
#[cfg(not(all(feature = "std", windows)))]
pub fn system_anchors() -> Vec<TrustAnchor> {
    Vec::new()
}

/// Windows keeps standard public roots in a *dynamic* store that Windows Update
/// grows, so the registry keys alone miss them (`Cert:\LocalMachine\Root` shows
/// 44 certificates while `ROOT\Certificates` holds 13 keys). `CertOpenSystemStore`
/// opens the same logical `ROOT` view the platform verifier uses, dynamic
/// entries included, and hands back each encoded certificate directly.
#[cfg(all(feature = "std", windows))]
mod windows_store {
    use alloc::vec::Vec;
    use core::{ffi::c_void, ptr};
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt};

    use super::super::{Certificate, TrustAnchor};

    /// `CERT_CONTEXT` from `wincrypt.h`.
    #[repr(C)]
    struct CertContext {
        encoding_type: u32,
        encoded: *mut u8,
        encoded_length: u32,
        info: *mut c_void,
        store: *mut c_void,
    }

    #[link(name = "crypt32")]
    unsafe extern "system" {
        fn CertOpenSystemStoreW(provider: usize, name: *const u16) -> *mut c_void;
        fn CertEnumCertificatesInStore(store: *mut c_void, previous: *const CertContext) -> *const CertContext;
        fn CertCloseStore(store: *mut c_void, flags: u32) -> i32;
    }

    /// NUL-terminated UTF-16 spelling of `text`.
    fn wide(text: &str) -> Vec<u16> {
        OsStr::new(text).encode_wide().chain(core::iter::once(0)).collect()
    }

    /// Every self-issued CA certificate in the logical `ROOT` store.
    pub(super) fn anchors() -> Vec<TrustAnchor> {
        let mut anchors: Vec<TrustAnchor> = Vec::new();
        let name = wide("ROOT");
        let store = unsafe { CertOpenSystemStoreW(0, name.as_ptr()) };
        if store.is_null() {
            return anchors;
        }
        let mut current: *const CertContext = ptr::null();
        loop {
            // Enumeration frees `current` before returning the next context.
            current = unsafe { CertEnumCertificatesInStore(store, current) };
            if current.is_null() {
                break;
            }
            let context = unsafe { &*current };
            if context.encoded.is_null() || context.encoded_length == 0 {
                continue;
            }
            let der = unsafe { core::slice::from_raw_parts(context.encoded, context.encoded_length as usize) };
            let Ok(cert) = Certificate::parse(der) else { continue };
            if !cert.extensions.is_ca || cert.subject.raw() != cert.issuer.raw() {
                continue;
            }
            let anchor = TrustAnchor::from_cert(&cert);
            if !anchors.iter().any(|existing| existing.subject_raw == anchor.subject_raw) {
                anchors.push(anchor);
            }
        }
        unsafe { CertCloseStore(store, 0) };
        anchors
    }
}

#[cfg(all(test, windows, feature = "std"))]
mod tests {
    #[test]
    fn reads_platform_roots_including_digicert() {
        let anchors = super::system_anchors();
        let labels: Vec<&str> = anchors.iter().map(|anchor| anchor.label.as_str()).collect();
        // `api.trae.cn` chains to DigiCert; the pinned Let's Encrypt set misses it.
        assert!(
            labels.iter().any(|label| label.contains("DigiCert")),
            "no DigiCert root among {} anchors: {labels:?}",
            anchors.len()
        );
    }
}
