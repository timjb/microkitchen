//! Size strings (`"4G"`, `"512MiB"`, `"2048"`) expressed in MiB.

use thiserror::Error;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// MiB per GiB.
pub const MIB_PER_GIB: u32 = 1024;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Why a size string could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SizeError {
    #[error("expected a size such as \"4G\" or \"512M\", got {0:?}")]
    Malformed(String),

    #[error("unknown unit {0:?}; use M, MB, MiB, G, GB or GiB")]
    UnknownUnit(String),

    #[error("the size must be greater than zero")]
    Zero,

    #[error("the size is too large")]
    TooLarge,
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Parse a size string into MiB.
///
/// Units are case-insensitive: `M`/`MB`/`MiB` and `G`/`GB`/`GiB`, all binary.
/// A bare integer is MiB. Whitespace and fractions are not accepted.
pub fn parse_size_mib(input: &str) -> Result<u32, SizeError> {
    let split = input
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(input.len());
    let (digits, unit) = input.split_at(split);
    if digits.is_empty() {
        return Err(SizeError::Malformed(input.to_owned()));
    }

    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "" | "m" | "mb" | "mib" => 1,
        "g" | "gb" | "gib" => MIB_PER_GIB,
        _ if unit.starts_with(|c: char| c == '.' || c.is_whitespace()) => {
            return Err(SizeError::Malformed(input.to_owned()));
        }
        _ => return Err(SizeError::UnknownUnit(unit.to_owned())),
    };

    let value: u32 = digits.parse().map_err(|_| SizeError::TooLarge)?;
    if value == 0 {
        return Err(SizeError::Zero);
    }
    value.checked_mul(multiplier).ok_or(SizeError::TooLarge)
}

/// Format MiB compactly: whole GiB as `"4G"`, anything else as `"1536M"`.
pub fn format_mib(mib: u32) -> String {
    if mib.is_multiple_of(MIB_PER_GIB) {
        format!("{}G", mib / MIB_PER_GIB)
    } else {
        format!("{mib}M")
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        let cases = [
            ("512", 512),
            ("512M", 512),
            ("512mb", 512),
            ("512MiB", 512),
            ("4G", 4096),
            ("4gb", 4096),
            ("4GiB", 4096),
            ("64G", 65536),
        ];
        for (input, mib) in cases {
            assert_eq!(parse_size_mib(input), Ok(mib), "{input}");
        }
    }

    #[test]
    fn rejects_bad_sizes() {
        assert_eq!(parse_size_mib(""), Err(SizeError::Malformed(String::new())));
        assert_eq!(parse_size_mib("G"), Err(SizeError::Malformed("G".into())));
        assert_eq!(
            parse_size_mib("1.5G"),
            Err(SizeError::Malformed("1.5G".into()))
        );
        assert_eq!(
            parse_size_mib("4 G"),
            Err(SizeError::Malformed("4 G".into()))
        );
        assert_eq!(
            parse_size_mib("4T"),
            Err(SizeError::UnknownUnit("T".into()))
        );
        assert_eq!(parse_size_mib("0G"), Err(SizeError::Zero));
        assert_eq!(parse_size_mib("99999999999"), Err(SizeError::TooLarge));
        assert_eq!(parse_size_mib("4194304G"), Err(SizeError::TooLarge));
    }

    #[test]
    fn formats_compactly() {
        assert_eq!(format_mib(4096), "4G");
        assert_eq!(format_mib(1536), "1536M");
    }
}
