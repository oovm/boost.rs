//! TLS 1.3 X25519 key exchange and the RFC 8446 §7.1 key schedule.
//!
//! The primitives come from `boost_crypto`: X25519, AES-128-GCM and HKDF are
//! called as byte-level functions, and every failure (including missing host
//! entropy) is returned instead of panicking.

use boost_crypto::{
    aead,
    mac::{hkdf_expand_sha256, hkdf_extract_sha256},
    secret::SecretBytes,
    x25519,
};
use boost_random::Secure;
use core::fmt;
use ml_kem::{Decapsulate, FromSeed, KeyExport, MlKem768, Seed};

use super::error::{TlsError, TlsResult};

/// An X25519 client key pair kept across a HelloRetryRequest.
pub struct KeyPair {
    secret: SecretBytes,
    public: [u8; 32],
}

impl fmt::Debug for KeyPair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("KeyPair").field("public", &self.public).finish()
    }
}

impl KeyPair {
    /// Generate a fresh ephemeral pair from the host CSPRNG.
    pub fn generate() -> TlsResult<Self> {
        let secret = x25519::generate_secret_key().map_err(|error| TlsError::Unsupported(format!("host entropy: {error}")))?;
        let public = x25519::public_key(&secret).map_err(|error| TlsError::Unsupported(format!("x25519 public key: {error}")))?;
        Ok(Self { secret, public })
    }

    /// The public value sent in `key_share`.
    pub fn public(&self) -> &[u8; 32] {
        &self.public
    }

    /// The 32-byte ECDH shared secret.
    pub fn shared(&self, peer: &[u8; 32]) -> TlsResult<[u8; 32]> {
        let shared = x25519::shared_secret(&self.secret, peer).map_err(|error| TlsError::Unsupported(format!("x25519 exchange: {error}")))?;
        let mut out = [0u8; 32];
        out.copy_from_slice(shared.expose());
        Ok(out)
    }
}

/// The hybrid group `X25519MLKEM768` (draft-ietf-tls-ecdhe-mlkem §2).
pub const GROUP_X25519_MLKEM768: u16 = 0x11EC;
/// An ML-KEM-768 encapsulation key, in bytes.
pub const MLKEM768_EK_LEN: usize = 1184;
/// An ML-KEM-768 ciphertext, in bytes.
pub const MLKEM768_CT_LEN: usize = 1088;
/// The client share: encapsulation key followed by the X25519 public key.
pub const HYBRID_CLIENT_SHARE_LEN: usize = MLKEM768_EK_LEN + 32;
/// The server share: ciphertext followed by the X25519 public key.
pub const HYBRID_SERVER_SHARE_LEN: usize = MLKEM768_CT_LEN + 32;

/// An `X25519MLKEM768` client key pair.
///
/// Both halves are fresh per handshake. The X25519 key is the one carried both
/// inside the hybrid share and alone, so a REALITY server that seals against
/// either one derives the same peer key.
pub struct HybridKeyPair {
    x25519: KeyPair,
    kem: ml_kem::DecapsulationKey<MlKem768>,
    encapsulation: Vec<u8>,
}

impl fmt::Debug for HybridKeyPair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("HybridKeyPair").field("public", self.x25519.public()).finish()
    }
}

impl HybridKeyPair {
    /// Generate a fresh pair from the host CSPRNG.
    pub fn generate() -> TlsResult<Self> {
        let x25519 = KeyPair::generate()?;
        let mut bytes = [0u8; 64];
        let mut entropy = Secure::system();
        entropy.fill(&mut bytes).map_err(|error| TlsError::Unsupported(format!("host entropy: {error}")))?;
        let seed = Seed::try_from(&bytes[..]).expect("64-byte ml-kem seed");
        let (kem, encapsulation) = <MlKem768 as FromSeed>::from_seed(&seed);
        Ok(Self { x25519, kem, encapsulation: encapsulation.to_bytes().to_vec() })
    }

    /// The X25519 public key, carried inside the hybrid share and on its own.
    pub fn x25519_public(&self) -> &[u8; 32] {
        self.x25519.public()
    }

    /// The hybrid `key_share` payload: encapsulation key ‖ X25519 public key.
    pub fn client_share(&self) -> Vec<u8> {
        let mut share = Vec::with_capacity(HYBRID_CLIENT_SHARE_LEN);
        share.extend_from_slice(&self.encapsulation);
        share.extend_from_slice(self.x25519.public());
        share
    }

    /// The 64-byte secret of the hybrid share: ML-KEM secret ‖ X25519 secret.
    pub fn hybrid_shared(&self, server_share: &[u8]) -> TlsResult<Vec<u8>> {
        if server_share.len() != HYBRID_SERVER_SHARE_LEN {
            return Err(TlsError::Protocol(format!("hybrid server share of {} bytes", server_share.len())));
        }
        let (ciphertext, peer) = server_share.split_at(MLKEM768_CT_LEN);
        let peer: &[u8; 32] = peer.try_into().expect("32-byte x25519 share");
        let secret = self.kem.decapsulate_slice(ciphertext).map_err(|error| TlsError::Protocol(format!("ml-kem decapsulation: {error}")))?;
        let mut shared = Vec::with_capacity(64);
        shared.extend_from_slice(&secret[..]);
        shared.extend_from_slice(&self.x25519.shared(peer)?);
        Ok(shared)
    }

    /// The bare X25519 exchange, used when a server picks group 29 instead.
    pub fn x25519_shared(&self, peer: &[u8; 32]) -> TlsResult<[u8; 32]> {
        self.x25519.shared(peer)
    }
}

/// X25519 on raw bytes: derive a pair from a fixed secret and print its public value.
#[cfg(test)]
fn public_from(secret: &[u8; 32]) -> TlsResult<[u8; 32]> {
    x25519::public_key(&SecretBytes::new(secret.to_vec())).map_err(|error| TlsError::Unsupported(format!("x25519 public key: {error}")))
}

/// X25519 on raw bytes, for vector testing.
#[cfg(test)]
fn shared_from(secret: &[u8; 32], peer: &[u8; 32]) -> TlsResult<[u8; 32]> {
    let shared = x25519::shared_secret(&SecretBytes::new(secret.to_vec()), peer)
        .map_err(|error| TlsError::Unsupported(format!("x25519 exchange: {error}")))?;
    let mut out = [0u8; 32];
    out.copy_from_slice(shared.expose());
    Ok(out)
}

/// HKDF-Extract with the given salt.
pub fn extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    hkdf_extract_sha256(Some(salt), ikm)
}

/// `Derive-Secret` from RFC 8446.
pub fn derive_secret(secret: &[u8], label: &str, context: &[u8]) -> TlsResult<[u8; 32]> {
    let out = expand_label(secret, label, context, 32)?;
    let mut derived = [0u8; 32];
    derived.copy_from_slice(&out);
    Ok(derived)
}

/// `HKDF-Expand-Label` from RFC 8446 §7.1.
pub fn expand_label(secret: &[u8], label: &str, context: &[u8], length: usize) -> TlsResult<Vec<u8>> {
    let full_label = format!("tls13 {label}");
    let mut info = Vec::with_capacity(3 + full_label.len() + context.len());
    info.extend_from_slice(&(length as u16).to_be_bytes());
    info.push(full_label.len() as u8);
    info.extend_from_slice(full_label.as_bytes());
    info.push(context.len() as u8);
    info.extend_from_slice(context);
    let mut out = vec![0u8; length];
    hkdf_expand_sha256(secret, &info, &mut out).map_err(|error| TlsError::Protocol(format!("hkdf expand: {error}")))?;
    Ok(out)
}

/// One direction of the AEAD record protection state.
#[derive(Debug, Clone, Copy)]
pub struct TrafficKey {
    key: [u8; 16],
    iv: [u8; 12],
    sequence: u64,
}

impl TrafficKey {
    /// Derive the key and IV from a traffic secret.
    pub fn from_secret(secret: &[u8]) -> TlsResult<Self> {
        let key_bytes = expand_label(secret, "key", &[], 16)?;
        let iv_bytes = expand_label(secret, "iv", &[], 12)?;
        let mut key = [0u8; 16];
        let mut iv = [0u8; 12];
        key.copy_from_slice(&key_bytes);
        iv.copy_from_slice(&iv_bytes);
        Ok(Self { key, iv, sequence: 0 })
    }

    fn next_nonce(&mut self) -> [u8; 12] {
        let mut nonce = self.iv;
        let seq = self.sequence.to_be_bytes();
        for (index, byte) in seq.iter().enumerate() {
            nonce[4 + index] ^= byte;
        }
        self.sequence += 1;
        nonce
    }

    /// Encrypt one record payload, returning the ciphertext with tag appended.
    pub fn seal(&mut self, plaintext: &[u8], aad: &[u8]) -> TlsResult<Vec<u8>> {
        let nonce = self.next_nonce();
        aead::aes_128_gcm_seal(&self.key, &nonce, aad, plaintext).map_err(|error| TlsError::Unsupported(format!("aes-128-gcm seal: {error}")))
    }

    /// Decrypt one record payload.
    pub fn open(&mut self, ciphertext: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
        let nonce = self.next_nonce();
        aead::aes_128_gcm_open(&self.key, &nonce, aad, ciphertext).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boost_crypto::encoding::hex_encode;

    #[test]
    fn expand_label_is_deterministic() {
        let secret = [7u8; 32];
        let first = derive_secret(&secret, "finished", &[0u8; 32]).expect("derive");
        let again = derive_secret(&secret, "finished", &[0u8; 32]).expect("derive");
        assert_eq!(hex_encode(&first), hex_encode(&again));
        assert_eq!(first.len(), 32);
    }

    #[test]
    fn expand_label_lengths_and_contexts() {
        let secret = [1u8; 32];
        assert_eq!(expand_label(&secret, "key", &[], 16).expect("key").len(), 16);
        assert_eq!(expand_label(&secret, "iv", &[], 12).expect("iv").len(), 12);
        let with_context = derive_secret(&secret, "c hs traffic", b"hash").expect("derive");
        let other = derive_secret(&secret, "c hs traffic", b"other").expect("derive");
        assert_ne!(with_context, other);
    }

    #[test]
    fn rfc8446_early_secret_matches_the_wire_vector() {
        // RFC 8448 §3: HKDF-Extract over an all-zero salt and IKM.
        let early = extract(&[0u8; 32], &[0u8; 32]);
        assert_eq!(hex_encode(&early), "33ad0a1c607ec03b09e6cd9893680ce210adf300aa1f2660e1b22e10f170f92a");
        let derived = derive_secret(&early, "derived", &SHA256_OF_EMPTY).expect("derive");
        assert_eq!(derived.len(), 32);
        let handshake = extract(&derived, &[0u8; 32]);
        assert_ne!(handshake, early);
    }

    /// SHA-256 of the empty string, the transcript hash before any message.
    const SHA256_OF_EMPTY: [u8; 32] = [
        0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b,
        0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
    ];

    #[test]
    fn hybrid_shared_secret_is_mlkem_then_x25519() {
        let pair = HybridKeyPair::generate().expect("pair");
        assert_eq!(pair.client_share().len(), HYBRID_CLIENT_SHARE_LEN);
        let peer = KeyPair::generate().expect("peer");
        let mut server_share = vec![0x5Au8; MLKEM768_CT_LEN];
        server_share.extend_from_slice(peer.public());
        let secret = pair.hybrid_shared(&server_share).expect("shared");
        assert_eq!(secret.len(), 64);
        // The X25519 half must land in the last 32 bytes, after the ML-KEM secret.
        assert_eq!(&secret[32..], peer.shared(pair.x25519_public()).expect("x25519"));
        assert!(pair.hybrid_shared(&server_share[..32]).is_err(), "a truncated share is rejected");
    }

    #[test]
    fn x25519_shared_secret_is_symmetric() {
        let a = KeyPair::generate().expect("key pair");
        let b = KeyPair::generate().expect("key pair");
        assert_eq!(a.shared(b.public()).expect("shared"), b.shared(a.public()).expect("shared"));
    }

    #[test]
    fn x25519_rfc7748_vector() {
        // RFC 7748 §6.1: Alice's and Bob's private/public pairs.
        let alice = [
            0x77, 0x07, 0x6d, 0x0a, 0x73, 0x18, 0xa5, 0x7d, 0x3c, 0x16, 0xc1, 0x72, 0x51, 0xb2, 0x66, 0x45, 0xdf, 0x4c, 0x2f, 0x87, 0xeb, 0xc0,
            0x99, 0x2a, 0xb1, 0x77, 0xfb, 0xa5, 0x1d, 0xb9, 0x2c, 0x2a,
        ];
        let bob = [
            0x5d, 0xab, 0x08, 0x7e, 0x62, 0x4a, 0x8a, 0x4b, 0x79, 0xe1, 0x7f, 0x8b, 0x83, 0x80, 0x0e, 0xe6, 0x6f, 0x3b, 0xb1, 0x29, 0x26, 0x18,
            0xb6, 0xfd, 0x1c, 0x2f, 0x8b, 0x27, 0xff, 0x88, 0xe0, 0xeb,
        ];
        let alice_public = public_from(&alice).expect("public");
        let bob_public = public_from(&bob).expect("public");
        assert_eq!(hex_encode(&alice_public), "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
        assert_eq!(hex_encode(&bob_public), "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
        let shared = "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742";
        assert_eq!(hex_encode(&shared_from(&alice, &bob_public).expect("shared")), shared);
        assert_eq!(hex_encode(&shared_from(&bob, &alice_public).expect("shared")), shared);
    }

    #[test]
    fn traffic_key_seal_open_round_trip() {
        let secret = [42u8; 32];
        let mut client = TrafficKey::from_secret(&secret).expect("key");
        let mut server = TrafficKey::from_secret(&secret).expect("key");
        let aad = [0x17u8, 0x03, 0x03, 0x00, 0x05];
        let sealed = client.seal(b"hello", &aad).expect("seal");
        let opened = server.open(&sealed, &aad).expect("open");
        assert_eq!(opened, b"hello");
    }

    #[test]
    fn sequence_nonce_advances() {
        let secret = [9u8; 32];
        let mut key = TrafficKey::from_secret(&secret).expect("key");
        let aad = [0x16u8, 0x03, 0x03, 0x00, 0x01];
        let first = key.seal(b"a", &aad).expect("seal");
        let second = key.seal(b"a", &aad).expect("seal");
        assert_ne!(first, second, "nonce must advance per record");
    }
}
