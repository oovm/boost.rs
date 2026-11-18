//! PostgreSQL authentication policy the gateway offers to a client.
//!
//! The methods and the MD5/SCRAM primitives are the protocol's, shared with the
//! client through `database-protocol-pgsql`. This module owns the policy: which
//! method a listener requires and which role/password it verifies, so the wire
//! session in [`super::connection`] only moves frames.

use database_protocol_pgsql::auth::md5_password;

/// Authentication a PostgreSQL listener requires before any statement runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgAuthentication {
    /// No password exchange: the loopback-only historical behaviour.
    Trust,
    /// `AuthenticationCleartextPassword` (`R` code 3).
    Password {
        /// Role the client must connect as.
        user: String,
        /// Expected password.
        password: String,
    },
    /// `AuthenticationMD5Password` (`R` code 5).
    Md5 {
        /// Role the client must connect as.
        user: String,
        /// Expected password.
        password: String,
    },
    /// `AuthenticationSASL` with SCRAM-SHA-256 (`R` codes 10/11/12).
    ScramSha256 {
        /// Role the client must connect as.
        user: String,
        /// Expected password.
        password: String,
    },
    /// Password exchange verified against the role's stored verifier in the
    /// cluster object catalog, so `CREATE ROLE ... PASSWORD` owns the credential
    /// instead of the listener configuration.
    RolePassword(PgPasswordMethod),
}

/// Wire method a role-password listener offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgPasswordMethod {
    /// `AuthenticationCleartextPassword` (`R` code 3).
    Cleartext,
    /// `AuthenticationMD5Password` (`R` code 5).
    Md5,
    /// `AuthenticationSASL` with SCRAM-SHA-256 (`R` codes 10/11/12).
    ScramSha256,
}

impl PgPasswordMethod {
    /// Method name reported by diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            Self::Cleartext => "password",
            Self::Md5 => "md5",
            Self::ScramSha256 => "scram-sha-256",
        }
    }
}

impl PgAuthentication {
    /// A listener that verifies logins against the role catalog.
    pub fn role_password(method: PgPasswordMethod) -> Self {
        Self::RolePassword(method)
    }

    /// The loopback-only default every existing listener keeps.
    pub fn trust() -> Self {
        Self::Trust
    }

    /// Configured role, when the method authenticates one.
    pub fn user(&self) -> Option<&str> {
        match self {
            Self::Trust | Self::RolePassword(_) => None,
            Self::Password { user, .. } | Self::Md5 { user, .. } | Self::ScramSha256 { user, .. } => Some(user),
        }
    }

    /// Configured password, when the method verifies one.
    pub fn password(&self) -> Option<&str> {
        match self {
            Self::Trust | Self::RolePassword(_) => None,
            Self::Password { password, .. } | Self::Md5 { password, .. } | Self::ScramSha256 { password, .. } => Some(password),
        }
    }

    /// Method name reported by `SHOW`-style diagnostics and failure messages.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Trust => "trust",
            Self::Password { .. } => "password",
            Self::Md5 { .. } => "md5",
            Self::ScramSha256 { .. } => "scram-sha-256",
            Self::RolePassword(method) => method.label(),
        }
    }

    /// Whether the startup `user` matches the configured role. A client naming
    /// another role is reported as a password failure, exactly like PostgreSQL
    /// reports a failed verification for the requested role.
    pub fn matches_role(&self, requested: &str) -> bool {
        self.user().is_none_or(|user| user == requested)
    }

    /// Expected `md5(md5(password || user) || salt)` response.
    pub fn expected_md5(&self, user: &str, salt: &[u8]) -> Option<String> {
        self.password().map(|password| md5_password(password, user, salt))
    }
}

/// One authentication failure, carrying the SQLSTATE the wire reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthFailure {
    /// SQLSTATE class, `28P01` for a failed password and `28000` for an
    /// unsupported method.
    pub sqlstate: &'static str,
    /// Human-readable detail.
    pub message: String,
}

impl AuthFailure {
    /// A rejected password, including an unknown role.
    pub fn password(user: &str) -> Self {
        Self { sqlstate: "28P01", message: format!("password authentication failed for user \"{user}\"") }
    }

    /// The gateway cannot run the requested mechanism.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self { sqlstate: "28000", message: message.into() }
    }

    /// A protocol-level failure inside the exchange.
    pub fn protocol(message: impl Into<String>) -> Self {
        Self { sqlstate: "08P01", message: message.into() }
    }
}
