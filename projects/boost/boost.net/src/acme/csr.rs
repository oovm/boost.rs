//! PKCS#10 requests and PKCS#8 private keys for ACME leaf certificates.
//!
//! ACME itself never inspects the CSR; it only forwards the DER to the CA. This
//! module gives every TLS product the same P-256 request shape — SAN DNS names,
//! digital signature key usage, and server authentication EKU — plus the PKCS#8
//! PEM every delivery path needs. Private key storage stays with the product.

use boost_crypto::ecdsa::p256::SigningKey;

use crate::pki::{EcCurve, PublicKey};

use data_binary_asn1::{Oid, Writer, oids};

use super::error::{AcmeError, AcmeResult};

/// Generate a fresh P-256 scalar (32 big-endian bytes) from the host CSPRNG.
pub fn generate_key() -> AcmeResult<[u8; 32]> {
    SigningKey::generate().map(|key| key.to_bytes()).map_err(|error| AcmeError::invalid_argument(error.to_string()))
}

fn signing_key(scalar: &[u8]) -> AcmeResult<SigningKey> {
    SigningKey::from_bytes(scalar).map_err(|error| AcmeError::invalid_argument(error.to_string()))
}

fn name_der(common_name: &str) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.sequence(|sequence| {
        sequence.set(|set| {
            set.sequence(|attribute| {
                attribute.write_oid(&oids::common_name());
                attribute.write_utf8(common_name);
            });
        });
    });
    writer.finish()
}

fn extensions_der(dns_names: &[String]) -> Vec<u8> {
    let mut san = Writer::new();
    san.sequence(|sequence| {
        for name in dns_names {
            sequence.context_primitive(2, name.as_bytes());
        }
    });
    let san = san.finish();

    let mut key_usage = Writer::new();
    key_usage.write_bitstring(&[0xa0, 0x00], 0);
    let key_usage = key_usage.finish();

    let mut eku = Writer::new();
    eku.sequence(|list| list.write_oid(&oids::server_auth()));
    let eku = eku.finish();

    let mut list = Writer::new();
    list.sequence(|outer| {
        for (oid, value) in [(&oids::subject_alt_name(), &san), (&oids::key_usage(), &key_usage), (&oids::extended_key_usage(), &eku)] {
            outer.sequence(|extension| {
                extension.write_oid(oid);
                extension.write_octetstring(value);
            });
        }
    });
    list.finish()
}

fn certification_request_info(common_name: &str, public_key: &PublicKey, dns_names: &[String]) -> Vec<u8> {
    let extensions = extensions_der(dns_names);
    let mut writer = Writer::new();
    writer.sequence(|info| {
        info.write_u64(0);
        info.write_raw(&name_der(common_name));
        info.write_raw(&public_key.to_der());
        info.context(0, |attributes| {
            attributes.sequence(|attribute| {
                attribute.write_oid(&oids::extension_request());
                attribute.set(|values| values.write_raw(&extensions));
            });
        });
    });
    writer.finish()
}

fn public_key(signing: &SigningKey) -> PublicKey {
    PublicKey::Ec { curve: EcCurve::P256, point: signing.verifying_key().to_sec1_bytes() }
}

fn ecdsa_sha256() -> Oid {
    oids::ecdsa_with_sha256()
}

/// Build a signed PKCS#10 request for the given names, returning DER.
pub fn create(scalar: &[u8], common_name: &str, dns_names: &[String]) -> AcmeResult<Vec<u8>> {
    let signing = signing_key(scalar)?;
    let info = certification_request_info(common_name, &public_key(&signing), dns_names);
    let signature = signing.sign_der(&info);
    let mut writer = Writer::new();
    writer.sequence(|request| {
        request.write_raw(&info);
        request.sequence(|algorithm| algorithm.write_oid(&ecdsa_sha256()));
        request.write_bitstring(&signature, 0);
    });
    Ok(writer.finish())
}

fn ec_private_key(signing: &SigningKey) -> Vec<u8> {
    let point = signing.verifying_key().to_sec1_bytes();
    let mut writer = Writer::new();
    writer.sequence(|key| {
        key.write_u64(1);
        key.write_octetstring(&signing.to_bytes());
        key.context(0, |parameters| parameters.write_oid(&oids::prime256v1()));
        key.context(1, |public| public.write_bitstring(&point, 0));
    });
    writer.finish()
}

/// Encode a P-256 scalar as a PKCS#8 `PRIVATE KEY` PEM document.
pub fn private_key_pem(scalar: &[u8]) -> AcmeResult<String> {
    let signing = signing_key(scalar)?;
    let inner = ec_private_key(&signing);
    let mut writer = Writer::new();
    writer.sequence(|info| {
        info.write_u64(0);
        info.sequence(|algorithm| {
            algorithm.write_oid(&oids::ec_public_key());
            algorithm.write_oid(&oids::prime256v1());
        });
        info.write_octetstring(&inner);
    });
    Ok(crate::pki::pem_encode("PRIVATE KEY", &writer.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csr_and_private_key_are_well_formed() {
        let scalar = [7u8; 32];
        let csr = create(&scalar, "example.test", &["example.test".to_owned(), "www.example.test".to_owned()]).expect("csr");
        assert!(csr.len() > 100);
        let pem = private_key_pem(&scalar).expect("pem");
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----\n") && pem.ends_with("-----END PRIVATE KEY-----\n"));
    }

    #[test]
    fn generated_keys_are_distinct_and_usable() {
        let first = generate_key().expect("key");
        let second = generate_key().expect("key");
        assert_ne!(first, second);
        assert!(create(&first, "example.test", &["example.test".to_owned()]).is_ok());
    }
}
