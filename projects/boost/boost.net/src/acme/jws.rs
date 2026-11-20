//! base64url, JWK thumbprints, and ES256 JWS construction for ACME requests.

use boost_crypto::{ecdsa::p256::SigningKey, encoding::base64url_encode, hash::sha256};
use serde_json::{Map, Value};

use super::error::{AcmeError, AcmeResult};

/// base64url without padding, as required by RFC 8555 §6.3.
pub fn b64url(bytes: &[u8]) -> String {
    base64url_encode(bytes)
}

/// The SHA-256 JWK thumbprint of an already canonical JWK string (RFC 8555 §8.1, RFC 7638).
pub fn jwk_thumbprint(canonical_jwk: &str) -> String {
    b64url(&sha256(canonical_jwk.as_bytes()))
}

/// The ES256 account key used for JWS signatures and dns-01 key authorizations.
///
/// The key stores its scalar so it stays cheap to clone across retries and
/// bounded polling loops; the signing key object is derived per signature.
#[derive(Clone, PartialEq, Eq)]
pub struct AccountKey {
    scalar: [u8; 32],
}

impl core::fmt::Debug for AccountKey {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_struct("AccountKey").field("thumbprint", &self.thumbprint()).finish()
    }
}

impl AccountKey {
    /// Generate a fresh P-256 account key from the host CSPRNG.
    pub fn generate() -> AcmeResult<Self> {
        SigningKey::generate()
            .map(|signing| Self { scalar: signing.to_bytes() })
            .map_err(|error| AcmeError::invalid_argument(error.to_string()))
    }

    /// Adopt an existing P-256 scalar (32 big-endian bytes).
    pub fn from_bytes(bytes: &[u8]) -> AcmeResult<Self> {
        SigningKey::from_bytes(bytes)
            .map(|signing| Self { scalar: signing.to_bytes() })
            .map_err(|error| AcmeError::invalid_argument(error.to_string()))
    }

    /// Export the P-256 scalar (32 big-endian bytes) for product-side key storage.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.scalar
    }

    fn signing_key(&self) -> SigningKey {
        SigningKey::from_bytes(&self.scalar).expect("a validated scalar stays valid")
    }

    /// The JWK object with only the required members `crv`, `kty`, `x`, `y`.
    fn jwk_value(&self) -> Value {
        let point = self.signing_key().verifying_key().to_sec1_bytes();
        let mut members = Map::new();
        members.insert("crv".to_owned(), Value::String("P-256".to_owned()));
        members.insert("kty".to_owned(), Value::String("EC".to_owned()));
        members.insert("x".to_owned(), Value::String(b64url(&point[1..33])));
        members.insert("y".to_owned(), Value::String(b64url(&point[33..65])));
        Value::Object(members)
    }

    /// The canonical JWK used for the thumbprint: required members only, lexicographic order.
    pub fn canonical_jwk(&self) -> String {
        self.jwk_value().to_string()
    }

    /// The account key fingerprint: `base64url(SHA-256(canonical JWK))`.
    pub fn thumbprint(&self) -> String {
        jwk_thumbprint(&self.canonical_jwk())
    }

    /// The key authorization string `token '.' thumbprint` for one challenge token.
    pub fn key_authorization(&self, token: &str) -> String {
        format!("{token}.{}", self.thumbprint())
    }
}

/// How a JWS identifies the ACME account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountRef {
    /// Self-certify with the embedded JWK, used before the account exists.
    Jwk,
    /// Reference the created account by its URL.
    Kid(String),
}

/// Build the flattened JWS JSON document for one ACME request.
///
/// `payload_json` is the raw JSON body, or the empty string for a POST-as-GET.
pub fn build_jws(key: &AccountKey, account: &AccountRef, url: &str, nonce: &str, payload_json: &str) -> AcmeResult<String> {
    let mut protected = Map::new();
    protected.insert("alg".to_owned(), Value::String("ES256".to_owned()));
    protected.insert("aud".to_owned(), Value::String(url.to_owned()));
    match account {
        AccountRef::Jwk => {
            protected.insert("jwk".to_owned(), key.jwk_value());
        }
        AccountRef::Kid(kid) => {
            protected.insert("kid".to_owned(), Value::String(kid.clone()));
        }
    }
    protected.insert("nonce".to_owned(), Value::String(nonce.to_owned()));
    protected.insert("url".to_owned(), Value::String(url.to_owned()));

    let protected = b64url(Value::Object(protected).to_string().as_bytes());
    let payload = b64url(payload_json.as_bytes());
    let signature = key.signing_key().sign_fixed(format!("{protected}.{payload}").as_bytes());

    let mut document = Map::new();
    document.insert("protected".to_owned(), Value::String(protected));
    document.insert("payload".to_owned(), Value::String(payload));
    document.insert("signature".to_owned(), Value::String(b64url(&signature)));
    serde_json::to_string(&Value::Object(document)).map_err(|error| AcmeError::protocol(error.to_string()))
}
