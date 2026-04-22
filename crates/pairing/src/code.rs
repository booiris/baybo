//! Short-code minting for the pairing gate.
//!
//! 6 characters from an unambiguous alphabet (no `0/O/1/I/L`). At 31
//! symbols × 6 positions the keyspace is ~887 M combinations; the
//! birthday collision probability at 100 concurrent pending codes is
//! ~6×10⁻⁶, well within a "pick-one, retry on collision" budget.

use rand::RngExt;
use thiserror::Error;

/// Length of a minted code. Exposed so callers can size buffers.
pub const CODE_LEN: usize = 6;

const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const _: () = assert!(ALPHABET.len() == 31);

#[derive(Debug, Error)]
pub enum CodeError {
    /// `generate_unique` couldn't find an unused code inside the retry
    /// budget. On a healthy system this is unreachable — it indicates
    /// the pending queue is so full the 1-billion-size keyspace is
    /// saturated, which would be a distinct operational problem.
    #[error("code generator exhausted retries ({0} attempts)")]
    Exhausted(u32),
}

/// Generate one random code. Stateless — callers that need
/// collision-free codes should use [`generate_unique`].
pub fn generate_code() -> String {
    let mut rng = rand::rng();
    let mut out = String::with_capacity(CODE_LEN);
    for _ in 0..CODE_LEN {
        let idx = rng.random_range(0..ALPHABET.len());
        out.push(ALPHABET[idx] as char);
    }
    out
}

/// Generate a code that `is_free` reports as available. Retries up to
/// `max_attempts` times before giving up — the service layer uses
/// this to avoid collisions with a live row's code.
///
/// `is_free` is called with each candidate; it returns `Ok(true)` if
/// the candidate is unused. Errors from the callback propagate
/// verbatim so the caller can distinguish "collision" from "storage
/// failed."
pub async fn generate_unique<F, Fut, E>(
    mut is_free: F,
    max_attempts: u32,
) -> Result<String, GenerateUniqueError<E>>
where
    F: FnMut(&str) -> Fut,
    Fut: std::future::Future<Output = Result<bool, E>>,
{
    for _ in 0..max_attempts {
        let candidate = generate_code();
        let free = is_free(&candidate)
            .await
            .map_err(GenerateUniqueError::Check)?;
        if free {
            return Ok(candidate);
        }
    }
    Err(GenerateUniqueError::Code(CodeError::Exhausted(
        max_attempts,
    )))
}

#[derive(Debug, Error)]
pub enum GenerateUniqueError<E> {
    #[error(transparent)]
    Code(#[from] CodeError),
    #[error("check collision callback: {0}")]
    Check(E),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_codes_have_expected_length() {
        for _ in 0..64 {
            assert_eq!(generate_code().len(), CODE_LEN);
        }
    }

    #[test]
    fn generated_codes_use_unambiguous_alphabet() {
        let disallowed = ['0', 'O', '1', 'I', 'L'];
        for _ in 0..256 {
            let code = generate_code();
            for ch in code.chars() {
                assert!(ch.is_ascii_alphanumeric(), "non-alnum: {ch}");
                assert!(!disallowed.contains(&ch), "ambiguous char leaked: {ch}");
            }
        }
    }

    #[tokio::test]
    async fn generate_unique_returns_first_free_candidate() {
        let out: Result<String, GenerateUniqueError<std::convert::Infallible>> =
            generate_unique(|_c| async { Ok(true) }, 3).await;
        let code = out.unwrap();
        assert_eq!(code.len(), CODE_LEN);
    }

    #[tokio::test]
    async fn generate_unique_exhausts_retries_when_never_free() {
        let out: Result<String, GenerateUniqueError<std::convert::Infallible>> =
            generate_unique(|_c| async { Ok(false) }, 3).await;
        assert!(matches!(
            out,
            Err(GenerateUniqueError::Code(CodeError::Exhausted(3)))
        ));
    }
}
