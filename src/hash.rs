//! The transaction hash, validated once at the edge.

use std::fmt;
use std::str::FromStr;

use serde::Serialize;

use crate::error::RewindError;

/// A transaction hash: 64 lowercase hexadecimal characters.
///
/// Parsing accepts the hash the way a person would paste it (optional `0x`,
/// any case, surrounding whitespace) and normalises it, because the RPC is
/// case-sensitive about hashes and always reports them lowercase.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct TxHash(String);

impl TxHash {
    /// The normalised hash: 64 lowercase hex characters, no `0x`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `abcdef…1234` — enough to recognise a hash in a header line.
    pub fn short(&self) -> String {
        format!("{}…{}", &self.0[..6], &self.0[self.0.len() - 4..])
    }
}

impl FromStr for TxHash {
    type Err = RewindError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let trimmed = raw.trim();
        let bare = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .unwrap_or(trimmed);

        if bare.len() != 64 || !bare.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(RewindError::InvalidHash {
                hash: raw.to_string(),
            });
        }
        Ok(TxHash(bare.to_ascii_lowercase()))
    }
}

impl fmt::Display for TxHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_hex_with_prefix_and_any_case() {
        let hash = "a".repeat(64);
        assert_eq!(hash.parse::<TxHash>().unwrap().as_str(), hash);
        assert_eq!(
            format!("0x{hash}").parse::<TxHash>().unwrap().as_str(),
            hash
        );
        assert_eq!(
            format!("  0X{}\n", hash.to_uppercase())
                .parse::<TxHash>()
                .unwrap()
                .as_str(),
            hash
        );
    }

    #[test]
    fn rejects_nonsense() {
        for bad in ["", "deadbeef", &"z".repeat(64), &"a".repeat(65)] {
            let err = bad.parse::<TxHash>().unwrap_err();
            assert!(
                err.to_string().contains("64 hexadecimal characters"),
                "{err}"
            );
        }
    }

    #[test]
    fn shortens_for_display() {
        let hash: TxHash = format!("abcdef{}1234", "0".repeat(54)).parse().unwrap();
        assert_eq!(hash.short(), "abcdef…1234");
    }
}
