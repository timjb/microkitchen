//! Strict hostname grammar.
//!
//! Applied to SOCKS5 domain-name fields and to configured rule entries. Input
//! that does not match is rejected, never repaired: no trimming, no truncation
//! at a null byte, no case folding into validity. A repair step is where the
//! decision engine and the connection layer end up judging different strings.

use thiserror::Error;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Maximum length of a hostname in its textual form, without a trailing dot.
pub const MAX_NAME_LEN: usize = 253;

/// Maximum length of a single label.
pub const MAX_LABEL_LEN: usize = 63;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Why a byte string is not an acceptable hostname.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum GrammarError {
    #[error("the name is empty")]
    Empty,

    #[error("the name is longer than {MAX_NAME_LEN} bytes")]
    TooLong,

    #[error("the name contains an empty label")]
    EmptyLabel,

    #[error("a label is longer than {MAX_LABEL_LEN} bytes")]
    LabelTooLong,

    #[error("a label starts or ends with '-'")]
    HyphenAtEdge,

    #[error("byte 0x{0:02x} is not allowed; only ASCII letters, digits, '-' and '.' are")]
    InvalidByte(u8),
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Validate `bytes` as an LDH hostname and return it as a string slice.
///
/// Labels are 1–63 bytes of ASCII letters, digits and hyphens, not starting or
/// ending with a hyphen; the whole name is at most 253 bytes. A trailing dot is
/// an empty label and therefore rejected.
pub fn validate_hostname(bytes: &[u8]) -> Result<&str, GrammarError> {
    if bytes.is_empty() {
        return Err(GrammarError::Empty);
    }
    if let Some(&byte) = bytes
        .iter()
        .find(|b| !(b.is_ascii_alphanumeric() || **b == b'-' || **b == b'.'))
    {
        return Err(GrammarError::InvalidByte(byte));
    }
    if bytes.len() > MAX_NAME_LEN {
        return Err(GrammarError::TooLong);
    }
    for label in bytes.split(|b| *b == b'.') {
        if label.is_empty() {
            return Err(GrammarError::EmptyLabel);
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(GrammarError::LabelTooLong);
        }
        if label.first() == Some(&b'-') || label.last() == Some(&b'-') {
            return Err(GrammarError::HyphenAtEdge);
        }
    }
    Ok(std::str::from_utf8(bytes).expect("validated as ASCII"))
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_names() {
        for name in [
            "example.com",
            "a",
            "api.github.com",
            "xn--bcher-kva.example",
            "EXAMPLE.com",
            "1password.com",
            "a-b.c-d",
        ] {
            assert_eq!(validate_hostname(name.as_bytes()), Ok(name), "{name}");
        }
    }

    #[test]
    fn rejects_without_repair() {
        let cases: &[(&[u8], GrammarError)] = &[
            (b"", GrammarError::Empty),
            (b"example.com\0.evil.com", GrammarError::InvalidByte(0)),
            (b"example.com\n", GrammarError::InvalidByte(b'\n')),
            (b" example.com", GrammarError::InvalidByte(b' ')),
            ("b\u{fc}cher.de".as_bytes(), GrammarError::InvalidByte(0xc3)),
            (b"under_score.com", GrammarError::InvalidByte(b'_')),
            (b"*.example.com", GrammarError::InvalidByte(b'*')),
            (b"example.com.", GrammarError::EmptyLabel),
            (b".example.com", GrammarError::EmptyLabel),
            (b"a..b", GrammarError::EmptyLabel),
            (b"-a.com", GrammarError::HyphenAtEdge),
            (b"a-.com", GrammarError::HyphenAtEdge),
        ];
        for (input, expected) in cases {
            assert_eq!(validate_hostname(input), Err(*expected), "{input:?}");
        }
    }

    #[test]
    fn enforces_length_limits() {
        let label63 = "a".repeat(63);
        assert!(validate_hostname(format!("{label63}.com").as_bytes()).is_ok());
        assert_eq!(
            validate_hostname(format!("a{label63}.com").as_bytes()),
            Err(GrammarError::LabelTooLong)
        );

        // 4 labels of 63 plus 3 dots = 255 bytes.
        let long = [label63.as_str(); 4].join(".");
        assert_eq!(
            validate_hostname(long.as_bytes()),
            Err(GrammarError::TooLong)
        );
        let max = format!("{}.{}", [label63.as_str(); 3].join("."), "a".repeat(61));
        assert_eq!(max.len(), MAX_NAME_LEN);
        assert!(validate_hostname(max.as_bytes()).is_ok());
    }
}
