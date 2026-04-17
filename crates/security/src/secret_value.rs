//! Redacted wrapper around secret bytes to keep plaintext out of `Debug`,
//! `Display`, and serialized output.

use crate::{Result, SecurityError};

#[derive(Clone)]
pub struct SecretValue {
    inner: Vec<u8>,
}

impl SecretValue {
    pub fn new(value: Vec<u8>) -> Self {
        Self { inner: value }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.inner
    }

    pub fn as_str(&self) -> Result<&str> {
        std::str::from_utf8(&self.inner)
            .map_err(|e| SecurityError::Encryption(format!("secret is not valid UTF-8: {e}")))
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl std::fmt::Display for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let sv = SecretValue::new(b"my-password".to_vec());
        assert_eq!(format!("{sv:?}"), "[REDACTED]");
    }

    #[test]
    fn display_is_redacted() {
        let sv = SecretValue::new(b"my-password".to_vec());
        assert_eq!(format!("{sv}"), "[REDACTED]");
    }
}
