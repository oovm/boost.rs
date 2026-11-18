//! PostgreSQL password authentication: MD5 and SCRAM-SHA-256.
//!
//! The algorithms live in `database-protocol-pgsql` so the gateway verifier and
//! this client run the same code. SASLprep is not applied; the common ASCII
//! password path is byte-identical without normalization.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use database_protocol_pgsql::auth::{hmac_sha256, md5_hex, pbkdf2_sha256, scram_escape};

pub(crate) use database_protocol_pgsql::auth::md5_password;

use super::error::PostgresError;

/// One SCRAM-SHA-256 exchange driven by the server's challenge messages.
pub(crate) struct ScramClient {
    client_first_bare: String,
    server_first: String,
    nonce: String,
    password: String,
    expected_server_signature: Option<[u8; 32]>,
}

impl ScramClient {
    /// Builds the client-first message and returns it for the SASL initial response.
    pub(crate) fn new(user: &str, password: &str) -> Result<(Self, String), PostgresError> {
        let mut random = [0u8; 18];
        boost_random::Secure::system()
            .fill(&mut random)
            .map_err(|error| PostgresError::Protocol(format!("cannot generate a SCRAM nonce: {error}")))?;
        Ok(Self::with_nonce(user, password, &STANDARD.encode(random)))
    }

    fn with_nonce(user: &str, password: &str, nonce: &str) -> (Self, String) {
        let client_first_bare = format!("n={},r={nonce}", scram_escape(user));
        let state = Self {
            client_first_bare: client_first_bare.clone(),
            server_first: String::new(),
            nonce: nonce.to_owned(),
            password: password.to_owned(),
            expected_server_signature: None,
        };
        (state, format!("n,,{client_first_bare}"))
    }

    /// Handles `SASLContinue` and returns the client-final message.
    pub(crate) fn continue_with(&mut self, server_first: &str) -> Result<String, PostgresError> {
        let mut nonce = None;
        let mut salt = None;
        let mut iterations = None;
        for field in server_first.split(',') {
            if let Some(value) = field.strip_prefix("r=") {
                nonce = Some(value.to_owned());
            }
            else if let Some(value) = field.strip_prefix("s=") {
                salt = Some(value.to_owned());
            }
            else if let Some(value) = field.strip_prefix("i=") {
                iterations = Some(value.parse::<u32>().map_err(|_| PostgresError::Protocol("invalid SCRAM iteration count".into()))?);
            }
        }
        let nonce = nonce.ok_or(PostgresError::Protocol("SCRAM server-first is missing the nonce".into()))?;
        let salt = salt.ok_or(PostgresError::Protocol("SCRAM server-first is missing the salt".into()))?;
        let iterations = iterations.ok_or(PostgresError::Protocol("SCRAM server-first is missing the iteration count".into()))?;
        if !nonce.starts_with(&self.nonce) {
            return Err(PostgresError::Protocol("SCRAM server nonce does not extend the client nonce".into()));
        }
        let decoded_salt =
            STANDARD.decode(&salt).map_err(|error| PostgresError::Protocol(format!("SCRAM salt is not valid base64: {error}")))?;
        let salted_password = pbkdf2_sha256(self.password.as_bytes(), &decoded_salt, iterations);
        self.server_first = server_first.to_owned();

        let client_key = hmac_sha256(&salted_password, b"Client Key");
        let stored_key: [u8; 32] = {
            use sha2::{Digest, Sha256};
            Sha256::digest(client_key).into()
        };
        let client_final_without_proof = format!("c=biws,r={nonce}");
        let auth_message = format!("{},{}", self.client_first_bare, self.server_first) + "," + &client_final_without_proof;
        let client_signature = hmac_sha256(&stored_key, auth_message.as_bytes());
        let mut proof = client_key;
        for (slot, value) in proof.iter_mut().zip(client_signature) {
            *slot ^= value;
        }
        let server_key = hmac_sha256(&salted_password, b"Server Key");
        self.expected_server_signature = Some(hmac_sha256(&server_key, auth_message.as_bytes()));
        Ok(format!("{client_final_without_proof},p={}", STANDARD.encode(proof)))
    }

    /// Verifies the `SASLFinal` server signature.
    pub(crate) fn verify(&self, server_final: &str) -> Result<(), PostgresError> {
        if let Some(error) = server_final.strip_prefix("e=") {
            return Err(PostgresError::Protocol(format!("SCRAM authentication failed: {error}")));
        }
        let signature = server_final.strip_prefix("v=").ok_or(PostgresError::Protocol("SCRAM server-final is missing the signature".into()))?;
        let decoded =
            STANDARD.decode(signature).map_err(|error| PostgresError::Protocol(format!("SCRAM signature is not valid base64: {error}")))?;
        let expected = self.expected_server_signature.ok_or(PostgresError::Protocol("SCRAM final arrived before continue".into()))?;
        if decoded != expected {
            return Err(PostgresError::Protocol("SCRAM server signature mismatch".into()));
        }
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gateway_verifier_accepts_a_client_exchange() {
        use database_protocol_pgsql::auth::ScramServer;

        let mut server = ScramServer::new("pencil", vec![9, 8, 7, 6, 5, 4, 3, 2], 4096, "SRVNONCE");
        let (mut client, client_first) = ScramClient::with_nonce("user", "pencil", "CLIENTNONCE");
        let server_first = server.begin(&client_first).expect("begin");
        let client_final = client.continue_with(&server_first).expect("continue");
        let server_final = server.finish(&client_final).expect("finish");
        client.verify(&server_final).expect("verify");
    }

    #[test]
    fn md5_matches_rfc1321_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(md5_hex(b"The quick brown fox jumps over the lazy dog"), "9e107d9d372bb6826bd81d3542a419d6");
    }

    #[test]
    fn md5_password_matches_the_postgres_algorithm() {
        let response = md5_password("secret", "postgres", &[1, 2, 3, 4]);
        let inner = md5_hex(b"secretpostgres");
        let mut material = inner.into_bytes();
        material.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(response, format!("md5{}", md5_hex(&material)));
    }

    #[test]
    fn pbkdf2_matches_rfc_6070_shape() {
        assert_eq!(
            pbkdf2_sha256(b"password", b"salt", 1),
            [
                0x12, 0x0f, 0xb6, 0xcf, 0xfc, 0xf8, 0xb3, 0x2c, 0x43, 0xe7, 0x22, 0x52, 0x56, 0xc4, 0xf8, 0x37, 0xa8, 0x65, 0x48, 0xc9, 0x2c,
                0xcc, 0x35, 0x48, 0x08, 0x05, 0x98, 0x7c, 0xb7, 0x0b, 0xe1, 0x7b
            ]
        );
    }

    #[test]
    fn scram_matches_rfc_7677_vector() {
        let nonce = "rOprNGfwEbeRWgbNEkqO";
        let (mut client, client_first) = ScramClient::with_nonce("user", "pencil", nonce);
        assert_eq!(client_first, format!("n,,n=user,r={nonce}"));
        let server_first = "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
        let client_final = client.continue_with(server_first).expect("continue");
        assert_eq!(client_final, "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=");
        client.verify("v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=").expect("verify");
    }

    #[test]
    fn scram_rejects_a_nonce_the_server_did_not_extend() {
        let (mut client, _) = ScramClient::with_nonce("user", "pencil", "abc");
        let server_first = "r=zzz,s=QSXCR+Q6sek8bf92,i=4096";
        assert!(client.continue_with(server_first).is_err());
    }

    #[test]
    fn scram_rejects_a_tampered_server_signature() {
        let (mut client, _) = ScramClient::with_nonce("user", "pencil", "abc");
        let server_first = "r=abc123,s=QSXCR+Q6sek8bf92,i=4096";
        client.continue_with(server_first).expect("continue");
        assert!(client.verify("v=rmF9pqV8S7suAoZWja4dJRkFsKQ=").is_err());
    }
}
