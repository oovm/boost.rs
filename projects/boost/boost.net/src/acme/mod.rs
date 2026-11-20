//! ACME (RFC 8555) protocol core shared by every OpenX TLS product.
//!
//! The module owns the wire documents, JWS signing, nonce handling, challenge
//! material, and retry policy. It deliberately does not own account keys on
//! disk, CSR generation, DNS provider credentials, certificate delivery, or
//! renewal scheduling: those stay with the consuming product.

pub mod client;
pub mod directory;
mod error;
pub mod jws;
pub mod types;

pub use client::{
    AcmeClient, BAD_NONCE, DnsChallenge, LOCATION_HEADER, NONCE_HEADER, OrderPollVerdict, RETRY_AFTER_HEADER, dns01_record_name, http01_path,
    order_poll_verdict, retry_after_delay, txt_digest,
};
pub use directory::{Directory, LETS_ENCRYPT_PRODUCTION, LETS_ENCRYPT_STAGING, parse_directory};
pub use error::{AcmeError, AcmeResult};
pub use jws::{AccountKey, AccountRef, b64url, build_jws, jwk_thumbprint};
pub use types::{
    Account, AccountStatus, Authorization, AuthorizationStatus, Challenge, ChallengeStatus, ChallengeType, Identifier, Order, OrderStatus,
    Problem, parse_account, parse_authorization, parse_challenge, parse_order,
};
