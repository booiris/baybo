//! Error surface for the push role.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PushError {
    /// The supplied `.p8` could not be parsed as a PKCS#8 P-256 key.
    #[error("apns key: {0}")]
    Key(String),

    /// ES256 signing of the provider token failed.
    #[error("jwt sign: {0}")]
    Jwt(String),
}
