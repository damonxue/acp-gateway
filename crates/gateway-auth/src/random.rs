//! Credential randomness, from the OS CSPRNG only.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use rand::RngCore;

/// A URL-safe random token carrying `bytes` bytes of entropy.
///
/// Used for nonces and ticket ids, both of which travel in query strings.
#[must_use]
pub fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buffer);
    BASE64URL.encode(buffer)
}

/// A uniformly distributed decimal code of `digits` digits, for humans.
///
/// Rejection sampling keeps every code equally likely; the modulo shortcut
/// would make low codes slightly more common, which is a real (if small) bias
/// in a credential.
#[must_use]
pub fn numeric_code(digits: u32) -> String {
    let modulus = 10u64.pow(digits);
    let limit = u64::MAX - (u64::MAX % modulus);
    let mut rng = rand::rngs::OsRng;
    loop {
        let value = rng.next_u64();
        if value < limit {
            return format!("{:0width$}", value % modulus, width = digits as usize);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_have_the_requested_width_and_are_numeric() {
        for _ in 0..64 {
            let code = numeric_code(6);
            assert_eq!(code.len(), 6);
            assert!(code.chars().all(|character| character.is_ascii_digit()));
        }
    }

    #[test]
    fn tokens_are_url_safe_and_unique() {
        let first = random_token(24);
        let second = random_token(24);
        assert_ne!(first, second);
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }
}
