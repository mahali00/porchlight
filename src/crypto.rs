use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::fmt::Write;
use subtle::ConstantTimeEq;

const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

pub fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);

    URL_SAFE_NO_PAD.encode(buffer)
}

pub fn sha256(value: &str) -> String {
    Sha256::digest(value.as_bytes()).iter().fold(String::with_capacity(64), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

fn hmac_block(key: &[u8]) -> [u8; 64] {
    let hashed;
    let source = if key.len() > 64 {
        hashed = Sha256::digest(key);
        hashed.as_slice()
    } else {
        key
    };
    let mut block = [0u8; 64];

    for (slot, byte) in block.iter_mut().zip(source) {
        *slot = *byte;
    }

    block
}

pub fn hmac(key: &str, value: &str) -> String {
    let mut mac = Hmac::<Sha256>::new(&hmac_block(key.as_bytes()).into());
    mac.update(value.as_bytes());

    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

pub fn safe_equal(left: &str, right: &str) -> bool {
    left.len() == right.len() && bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn approval_code() -> String {
    (0..8)
        .filter_map(|_| CODE_ALPHABET.get(rand::random_range(0..CODE_ALPHABET.len())))
        .map(|byte| char::from(*byte))
        .collect()
}

pub fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| !c.is_whitespace() && *c != '-').flat_map(char::to_uppercase).collect()
}

pub fn parse_duration(input: &str) -> Option<i64> {
    let trimmed = input.trim();
    let (digits, unit) = trimmed.split_at_checked(trimmed.len().checked_sub(1)?)?;

    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    let scale = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };

    digits.parse::<i64>().ok()?.checked_mul(scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_seals_match_the_typescript_build() {
        assert_eq!(
            hmac("key", "The quick brown fox jumps over the lazy dog"),
            "97yD9DBThCSxMpjmqm-xQ-9NWaFJRhdZl0edvC0aPNg"
        );
    }
}
