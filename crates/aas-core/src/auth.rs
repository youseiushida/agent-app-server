//! Tokens, pairing codes and rate limiting.

use std::time::{Duration, Instant};

use base64::Engine as _;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Crockford-style alphabet without ambiguous characters (no 0/O, 1/I/L, U).
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("OS random source available");
    buf
}

/// A new device token (256 bits, base64url without padding).
pub fn new_token() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

/// Stored form of a token.
pub fn hash_secret(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// Constant-time comparison of two secrets / hashes.
pub fn secrets_equal(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// A new pairing code `XXXX-XXXX` (~39 bits of entropy; single use, short-lived, rate limited).
pub fn new_pairing_code() -> String {
    let bytes = random_bytes::<8>();
    let chars: String = bytes
        .iter()
        .map(|b| CODE_ALPHABET[*b as usize % CODE_ALPHABET.len()] as char)
        .collect();
    format!("{}-{}", &chars[..4], &chars[4..])
}

/// Canonical form of a typed code (case, dashes and spaces ignored).
pub fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

pub fn hash_code(code: &str) -> String {
    hash_secret(&normalize_code(code))
}

/// Fixed-window limiter: at most `limit` attempts per `window` (`policy.pairing_rate_window`).
pub struct RateLimiter {
    limit: u32,
    window: Duration,
    started: Instant,
    count: u32,
}

impl RateLimiter {
    pub fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            started: Instant::now(),
            count: 0,
        }
    }

    /// Records an attempt; `false` when the limit is exceeded.
    pub fn allow(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.started) >= self.window {
            self.started = now;
            self.count = 0;
        }
        if self.count >= self.limit {
            return false;
        }
        self.count += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_and_codes_have_expected_shapes() {
        let t = new_token();
        assert_eq!(t.len(), 43);
        assert_ne!(t, new_token());
        let c = new_pairing_code();
        assert_eq!(c.len(), 9);
        assert_eq!(&c[4..5], "-");
        assert!(
            c.chars()
                .filter(|c| *c != '-')
                .all(|ch| CODE_ALPHABET.contains(&(ch as u8)))
        );
        assert_eq!(
            hash_code(&c),
            hash_code(&c.to_lowercase().replace('-', " "))
        );
    }

    #[test]
    fn constant_time_compare() {
        assert!(secrets_equal("abc", "abc"));
        assert!(!secrets_equal("abc", "abd"));
        assert!(!secrets_equal("abc", "abcd"));
    }

    #[test]
    fn limiter_blocks_after_limit_until_the_window_has_passed() {
        let window = Duration::from_millis(200);
        let mut l = RateLimiter::new(2, window);
        assert!(l.allow());
        assert!(l.allow());
        assert!(!l.allow());
        std::thread::sleep(window + Duration::from_millis(50));
        assert!(l.allow(), "a new window starts");
    }
}
