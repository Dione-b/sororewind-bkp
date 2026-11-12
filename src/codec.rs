//! base64 and XDR plumbing shared by every stage.
//!
//! The RPC speaks base64-encoded XDR and the host speaks raw XDR; this module
//! is the only place that converts between the two, so every failure names
//! the field it came from.

use base64::Engine;
use stellar_xdr::{LedgerEntry, LedgerEntryData, LedgerEntryExt, Limits, ReadXdr, WriteXdr};

use crate::error::{Result, RewindError};
use crate::rpc::RawLedgerEntry;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Decode standard base64, ignoring surrounding whitespace; `what` names
/// the field in the error.
pub fn b64_decode(text: &str, what: &'static str) -> Result<Vec<u8>> {
    B64.decode(text.trim())
        .map_err(|source| RewindError::Base64 { what, source })
}

/// Encode bytes as standard, padded base64.
pub fn b64_encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

/// Decode a base64 XDR value the RPC returned.
pub fn decode<T: ReadXdr>(text: &str, what: &'static str) -> Result<T> {
    let bytes = b64_decode(text, what)?;
    T::from_xdr(&bytes, Limits::none()).map_err(|source| RewindError::XdrDecode { what, source })
}

/// Encode a value into the raw XDR the host consumes.
pub fn encode<T: WriteXdr>(value: &T, what: &'static str) -> Result<Vec<u8>> {
    value
        .to_xdr(Limits::none())
        .map_err(|source| RewindError::XdrEncode { what, source })
}

/// `getLedgerEntries` hands back an entry as separate pieces — the body in
/// `xdr` (a `LedgerEntryData`), the extension in `extXdr`, and the bookkeeping
/// fields alongside. Reassemble them into the `LedgerEntry` the host wants.
pub fn assemble_entry(raw: &RawLedgerEntry) -> Result<LedgerEntry> {
    let data: LedgerEntryData = decode(&raw.xdr, "a ledger entry body")?;

    // Older RPCs omit the field; an absent extension is replayed as `V0`,
    // and `state` reports the classic entries it may have shrunk. Likewise an
    // absent sequence is `0` for the host, and reported as unknown drift.
    let ext = match raw.ext_xdr.as_deref() {
        Some(text) if !text.is_empty() => decode(text, "a ledger entry extension")?,
        _ => LedgerEntryExt::V0,
    };

    Ok(LedgerEntry {
        last_modified_ledger_seq: raw.last_modified_ledger_seq.unwrap_or(0),
        data,
        ext,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{account_entry, raw_entry};
    use stellar_xdr::LedgerKey;

    #[test]
    fn round_trips_values() {
        let key = LedgerKey::ContractCode(stellar_xdr::LedgerKeyContractCode {
            hash: stellar_xdr::Hash([7; 32]),
        });
        let text = b64_encode(&encode(&key, "key").unwrap());
        assert_eq!(decode::<LedgerKey>(&text, "key").unwrap(), key);
        // Whitespace around the payload is tolerated.
        assert_eq!(
            decode::<LedgerKey>(&format!(" {text}\n"), "key").unwrap(),
            key
        );
    }

    #[test]
    fn names_the_field_that_failed() {
        let err = decode::<LedgerKey>("!!!", "envelopeXdr").unwrap_err();
        assert!(matches!(
            err,
            RewindError::Base64 {
                what: "envelopeXdr",
                ..
            }
        ));
        assert!(err.to_string().contains("invalid base64 in envelopeXdr"));

        let err = decode::<LedgerKey>("AAAA", "resultXdr").unwrap_err();
        assert!(matches!(
            err,
            RewindError::XdrDecode {
                what: "resultXdr",
                ..
            }
        ));
    }

    #[test]
    fn assembles_an_entry_from_its_pieces() {
        let entry = account_entry([1; 32], 77);
        let raw = raw_entry(&entry, None);
        assert_eq!(assemble_entry(&raw).unwrap(), entry);

        // An absent or empty extension is `V0`; an absent sequence is 0.
        let mut bare = raw.clone();
        bare.ext_xdr = None;
        bare.last_modified_ledger_seq = None;
        let assembled = assemble_entry(&bare).unwrap();
        assert_eq!(assembled.ext, LedgerEntryExt::V0);
        assert_eq!(assembled.last_modified_ledger_seq, 0);

        let mut broken = raw;
        broken.xdr = "AAAA".into();
        assert!(matches!(
            assemble_entry(&broken),
            Err(RewindError::XdrDecode { .. })
        ));
    }
}
