//! spec: read a contract's own vocabulary out of its WASM.
//!
//! When a contract fails with `ScError::Contract(n)` the host only tells us a
//! number. The name lives in the contract's own `contractspecv0` section, so
//! that is where we go to turn `#3` into `InsufficientBalance`.

use std::collections::HashMap;
use std::io::Cursor;

use stellar_xdr::{
    ContractDataDurability, ContractExecutable, ContractId, LedgerEntry, LedgerEntryData,
    LedgerKey, LedgerKeyContractCode, LedgerKeyContractData, Limited, Limits, ReadXdr, ScAddress,
    ScSpecEntry, ScVal,
};

use crate::codec;
use crate::error::{Result, RewindError};
use crate::rpc::RpcClient;
use crate::state::Reconstructed;

/// Contract-declared error names, keyed by the numeric code an
/// `ScError::Contract(n)` carries.
///
/// A contract can declare several error enums (its own, and those of
/// contracts it imports), and they may reuse numbers; every declaration is
/// kept so an ambiguous code is reported as such rather than guessed.
#[derive(Debug, Default, Clone)]
pub struct ErrorNames {
    /// Per code, each `(enum, case)` that declares it, in spec order.
    names: HashMap<u32, Vec<(String, String)>>,
    /// The names are the Stellar Asset Contract's, not read from a spec.
    builtin: bool,
}

/// The name of one error code, and where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorName {
    /// The case name, or `Enum::Case or Enum::Case` when several enums
    /// declare the code.
    pub name: String,
    /// Where the name came from, worded for the report: the spec enum, the
    /// SAC's built-in errors, or how many enums claim the code.
    pub origin: String,
}

impl ErrorNames {
    /// The name declared for `code`. When several enums declare the code
    /// under different names, all of them, qualified.
    pub fn lookup(&self, code: u32) -> Option<ErrorName> {
        match self.names.get(&code)?.as_slice() {
            [] => None,
            [(_, case)] if self.builtin => Some(ErrorName {
                name: case.clone(),
                origin: "the Stellar Asset Contract's built-in errors".to_string(),
            }),
            [(error_enum, case)] => Some(ErrorName {
                name: case.clone(),
                origin: format!("spec enum {error_enum}"),
            }),
            candidates => Some(ErrorName {
                name: candidates
                    .iter()
                    .map(|(error_enum, case)| format!("{error_enum}::{case}"))
                    .collect::<Vec<_>>()
                    .join(" or "),
                origin: format!(
                    "{} spec enums that declare this code; which one raised it is not recorded",
                    candidates.len()
                ),
            }),
        }
    }

    /// Just the name from [`ErrorNames::lookup`], without its origin.
    pub fn name(&self, code: u32) -> Option<String> {
        self.lookup(code).map(|found| found.name)
    }
}

/// The errors of the built-in Stellar Asset Contract, which has no WASM and
/// so no spec: `ContractError` in soroban-env-host's
/// `builtin_contracts/contract_error.rs`, identical in hosts 29.0.2 and
/// 30.0.0 (checked 2026-11-11). Review it with each protocol upgrade.
pub fn stellar_asset_contract() -> ErrorNames {
    const ERRORS: [(u32, &str); 14] = [
        (2, "OperationNotSupportedError"),
        (3, "AlreadyInitializedError"),
        (4, "UnauthorizedError"),
        (5, "AuthenticationError"),
        (6, "AccountMissingError"),
        (7, "AccountIsNotClassic"),
        (8, "NegativeAmountError"),
        (9, "AllowanceError"),
        (10, "BalanceError"),
        (11, "BalanceDeauthorizedError"),
        (12, "OverflowError"),
        (13, "TrustlineMissingError"),
        (14, "InsufficientAccountReserve"),
        (15, "TooManyAccountSubentries"),
    ];
    ErrorNames {
        names: ERRORS
            .iter()
            .map(|(code, name)| (*code, vec![("ContractError".to_string(), name.to_string())]))
            .collect(),
        builtin: true,
    }
}

/// Read the error enums out of a WASM's `contractspecv0` custom section.
///
/// The section is a bare stream of `ScSpecEntry` XDR values with no framing,
/// so it is decoded entry by entry until the bytes run out.
pub fn from_wasm(wasm: &[u8]) -> Result<ErrorNames> {
    let missing = |reason: String| RewindError::SpecMissing { reason };
    let section = custom_section(wasm, "contractspecv0")
        .ok_or_else(|| missing("the WASM has no `contractspecv0` section".to_string()))?;

    let mut reader = Limited::new(
        Cursor::new(section),
        Limits {
            depth: 500,
            len: 0x100_0000,
        },
    );

    let mut names: HashMap<u32, Vec<(String, String)>> = HashMap::new();
    for entry in ScSpecEntry::read_xdr_iter(&mut reader) {
        let entry = entry
            .map_err(|e| missing(format!("the `contractspecv0` section is malformed: {e}")))?;
        if let ScSpecEntry::UdtErrorEnumV0(error_enum) = entry {
            let enum_name = String::from_utf8_lossy(&error_enum.name).into_owned();
            for case in error_enum.cases.iter() {
                let declared = (
                    enum_name.clone(),
                    String::from_utf8_lossy(&case.name).into_owned(),
                );
                let known = names.entry(case.value).or_default();
                // The same enum imported twice is one declaration.
                if !known.contains(&declared) {
                    known.push(declared);
                }
            }
        }
    }

    if names.is_empty() {
        return Err(missing(
            "the `contractspecv0` section declares no error enum".to_string(),
        ));
    }

    Ok(ErrorNames {
        names,
        builtin: false,
    })
}

/// Locate a custom section in a WASM binary.
///
/// Decision (hand-rolled, kept): the binary format is a flat sequence of
/// `id + LEB128 size + payload`, and this only ever needs to find one custom
/// section, so a dozen lines beat pulling in `wasmparser`. The input is bytes
/// fetched from the network, so the walker must never panic or read out of
/// bounds on arbitrary input; the tests below pin that down, including a
/// randomised sweep.
fn custom_section<'a>(wasm: &'a [u8], wanted: &str) -> Option<&'a [u8]> {
    const HEADER: usize = 8; // magic + version
    if wasm.len() < HEADER || &wasm[..4] != b"\0asm" {
        return None;
    }
    let mut at = HEADER;
    while at < wasm.len() {
        let id = wasm[at];
        let (size, size_len) = leb128_u32(&wasm[at + 1..])?;
        let payload = at + 1 + size_len;
        let end = payload.checked_add(size as usize)?;
        if end > wasm.len() {
            return None;
        }
        if id == 0 {
            // A custom section's payload starts with its name.
            let (name_len, name_len_len) = leb128_u32(&wasm[payload..end])?;
            let name = payload + name_len_len;
            let name_end = name.checked_add(name_len as usize)?;
            if name_end <= end && &wasm[name..name_end] == wanted.as_bytes() {
                return Some(&wasm[name_end..end]);
            }
        }
        at = end;
    }
    None
}

/// Decode an unsigned LEB128 `u32`, returning the value and its length.
///
/// As the WebAssembly spec has it: at most five bytes, padded encodings are
/// fine, and bits that do not fit in 32 are an error.
fn leb128_u32(bytes: &[u8]) -> Option<(u32, usize)> {
    let mut value = 0u32;
    for (i, byte) in bytes.iter().take(5).enumerate() {
        let bits = u32::from(byte & 0x7f);
        // The fifth byte contributes only the top four bits of a u32.
        if i == 4 && bits > 0x0f {
            return None;
        }
        value |= bits << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// What executes a contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractCode {
    /// A WASM contract, with the bytes the ledger holds for it.
    Wasm(ContractWasm),
    /// The built-in Stellar Asset Contract: no WASM, no spec.
    StellarAsset,
}

/// A contract's WASM as the ledger has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractWasm {
    /// The contract code entry's WASM bytes.
    pub wasm: Vec<u8>,
    /// The contract's instance or code was written at or after the
    /// transaction's ledger (and could not be rebuilt): it may have been
    /// upgraded since, so this may not be the code the transaction ran.
    pub changed_since: bool,
}

/// Fetch the WASM behind a contract: its instance entry tells us the code
/// hash, the code entry gives us the bytes. Both are usually already in the
/// footprint; when they are not, they are one `getLedgerEntries` away each.
pub async fn contract_wasm(
    rpc: &RpcClient,
    state: &Reconstructed,
    contract_id: &ContractId,
) -> Result<ContractCode> {
    let instance_key = LedgerKey::ContractData(LedgerKeyContractData {
        contract: ScAddress::Contract(contract_id.clone()),
        key: ScVal::LedgerKeyContractInstance,
        durability: ContractDataDurability::Persistent,
    });
    let (instance, instance_changed) = fetch_entry(rpc, state, &instance_key).await?;

    let LedgerEntryData::ContractData(data) = instance.data else {
        return Err(RewindError::ContractCode {
            reason: "the contract's instance entry is not contract data",
        });
    };
    let ScVal::ContractInstance(instance) = data.val else {
        return Err(RewindError::ContractCode {
            reason: "the contract's instance entry does not hold a contract instance",
        });
    };
    let ContractExecutable::Wasm(hash) = instance.executable else {
        return Ok(ContractCode::StellarAsset);
    };

    let code_key = LedgerKey::ContractCode(LedgerKeyContractCode { hash });
    let (code, code_changed) = fetch_entry(rpc, state, &code_key).await?;
    let LedgerEntryData::ContractCode(code) = code.data else {
        return Err(RewindError::ContractCode {
            reason: "the contract's code entry does not hold contract code",
        });
    };
    Ok(ContractCode::Wasm(ContractWasm {
        wasm: code.code.to_vec(),
        changed_since: instance_changed || code_changed,
    }))
}

/// Look a ledger entry up in the reconstructed footprint first, and only go to
/// the network when the transaction never touched it. Also says whether the
/// entry was written at or after the transaction's ledger.
async fn fetch_entry(
    rpc: &RpcClient,
    state: &Reconstructed,
    key: &LedgerKey,
) -> Result<(LedgerEntry, bool)> {
    let key_xdr = codec::encode(key, "a ledger key")?;
    let tx_ledger = state.ledger_info.sequence_number;

    if let Some(slot) = state.slot(&key_xdr) {
        let entry = slot.entry.clone().ok_or(RewindError::ContractCode {
            reason: "the entry is no longer on the ledger",
        })?;
        return Ok((entry, slot.value_drifted(tx_ledger)));
    }

    let keys = [codec::b64_encode(&key_xdr)];
    let raw =
        rpc.get_ledger_entries(&keys)
            .await?
            .pop()
            .flatten()
            .ok_or(RewindError::ContractCode {
                reason: "the RPC has no such entry; the contract may have been archived",
            })?;
    let changed = raw.last_modified_ledger_seq.is_some_and(|m| m >= tx_ledger);
    Ok((codec::assemble_entry(&raw)?, changed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{spec_section, wasm_with_sections};

    #[test]
    fn leb128_decodes_per_the_wasm_spec() {
        assert_eq!(leb128_u32(&[0x00]), Some((0, 1)));
        assert_eq!(leb128_u32(&[0x7f]), Some((127, 1)));
        assert_eq!(leb128_u32(&[0xe5, 0x8e, 0x26]), Some((624_485, 3)));
        assert_eq!(
            leb128_u32(&[0xff, 0xff, 0xff, 0xff, 0x0f]),
            Some((u32::MAX, 5))
        );
        // Trailing bytes are left alone.
        assert_eq!(leb128_u32(&[0x01, 0xff]), Some((1, 1)));
        // Padded (over-long but in-range) encodings are legal.
        assert_eq!(leb128_u32(&[0x80, 0x00]), Some((0, 2)));
        assert_eq!(leb128_u32(&[0x81, 0x80, 0x80, 0x80, 0x00]), Some((1, 5)));
    }

    #[test]
    fn leb128_rejects_bad_encodings() {
        // Truncated: the continuation bit promises a byte that is not there.
        assert_eq!(leb128_u32(&[]), None);
        assert_eq!(leb128_u32(&[0x80]), None);
        assert_eq!(leb128_u32(&[0xff, 0xff]), None);
        // More than five bytes.
        assert_eq!(leb128_u32(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]), None);
        // Five bytes whose last one overflows 32 bits.
        assert_eq!(leb128_u32(&[0xff, 0xff, 0xff, 0xff, 0x10]), None);
        assert_eq!(leb128_u32(&[0xff, 0xff, 0xff, 0xff, 0x7f]), None);
    }

    #[test]
    fn finds_the_wanted_custom_section() {
        let wasm = wasm_with_sections(&[
            (1, b"type section payload".to_vec()),
            (0, [&[5u8][..], b"other", b"xx"].concat()),
            (0, [&[14u8][..], b"contractspecv0", b"SPEC"].concat()),
        ]);
        assert_eq!(custom_section(&wasm, "contractspecv0"), Some(&b"SPEC"[..]));
        assert_eq!(custom_section(&wasm, "other"), Some(&b"xx"[..]));
        assert_eq!(custom_section(&wasm, "missing"), None);
    }

    #[test]
    fn rejects_malformed_binaries_without_panicking() {
        // Not WASM, or too short to have sections.
        assert_eq!(custom_section(b"", "x"), None);
        assert_eq!(custom_section(b"\0asm", "x"), None);
        assert_eq!(custom_section(b"notwasm!\0\x01x", "x"), None);

        let header = b"\0asm\x01\0\0\0";
        // A section id with nothing after it.
        assert_eq!(custom_section(&[&header[..], &[0]].concat(), "x"), None);
        // A section length past the end of the file.
        assert_eq!(
            custom_section(&[&header[..], &[0, 50, 1, b'x']].concat(), "x"),
            None
        );
        // A section length that is an invalid LEB128.
        assert_eq!(
            custom_section(
                &[&header[..], &[0, 0xff, 0xff, 0xff, 0xff, 0x7f]].concat(),
                "x"
            ),
            None
        );
        // A name length past the end of its section.
        assert_eq!(
            custom_section(&[&header[..], &[0, 2, 9, b'x']].concat(), "x"),
            None
        );
        // An empty custom section has no name to read.
        assert_eq!(custom_section(&[&header[..], &[0, 0]].concat(), "x"), None);
        // A truncated section after a valid one.
        let wasm = [&header[..], &[1, 1, 0xaa, 0, 3, 1]].concat();
        assert_eq!(custom_section(&wasm, "x"), None);
    }

    /// Property: arbitrary bytes never panic, and a returned section always
    /// lies inside the input. Deterministic xorshift so failures reproduce.
    #[test]
    fn arbitrary_input_never_panics() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let valid = wasm_with_sections(&[
            (1, vec![1, 2, 3]),
            (0, [&[14u8][..], b"contractspecv0", b"SPEC"].concat()),
        ]);
        for round in 0..20_000 {
            let mut bytes = if round % 2 == 0 {
                // Mutate a valid binary: flip bytes, then maybe truncate.
                let mut bytes = valid.clone();
                for _ in 0..=(next() % 4) {
                    let at = (next() as usize) % bytes.len();
                    bytes[at] = next() as u8;
                }
                bytes
            } else {
                // Random bytes behind a valid header.
                let mut bytes = b"\0asm\x01\0\0\0".to_vec();
                bytes.extend((0..next() % 64).map(|_| next() as u8));
                bytes
            };
            bytes.truncate(1 + (next() as usize) % bytes.len());

            if let Some(section) = custom_section(&bytes, "contractspecv0") {
                let start = section.as_ptr() as usize - bytes.as_ptr() as usize;
                assert!(start + section.len() <= bytes.len());
            }
            let _ = from_wasm(&bytes);
            let _ = leb128_u32(&bytes);
        }
    }

    #[test]
    fn reads_error_names_from_the_spec() {
        let section = spec_section("Error", &[("NotFound", 1), ("AlreadyProcessed", 13)]);
        let wasm = wasm_with_sections(&[(0, [&[14u8][..], b"contractspecv0", &section].concat())]);
        let names = from_wasm(&wasm).unwrap();
        assert_eq!(names.name(13).as_deref(), Some("AlreadyProcessed"));
        assert_eq!(names.name(2), None);
        assert_eq!(
            names.lookup(1),
            Some(ErrorName {
                name: "NotFound".to_string(),
                origin: "spec enum Error".to_string(),
            })
        );
    }

    #[test]
    fn several_error_enums_are_kept_apart() {
        let section = [
            spec_section("Error", &[("NotFound", 1), ("Paused", 2)]),
            spec_section("Pool", &[("Empty", 1)]),
            // The same enum imported a second time declares nothing new.
            spec_section("Error", &[("NotFound", 1)]),
        ]
        .concat();
        let wasm = wasm_with_sections(&[(0, [&[14u8][..], b"contractspecv0", &section].concat())]);
        let names = from_wasm(&wasm).unwrap();

        // Each unambiguous code names the enum it came from, not the last one.
        assert_eq!(names.lookup(2).unwrap().origin, "spec enum Error");
        // A shared code lists every candidate instead of picking one.
        let shared = names.lookup(1).unwrap();
        assert_eq!(shared.name, "Error::NotFound or Pool::Empty");
        assert!(
            shared.origin.starts_with("2 spec enums"),
            "{}",
            shared.origin
        );
    }

    #[test]
    fn the_stellar_asset_contract_has_builtin_names() {
        let names = stellar_asset_contract();
        let balance = names.lookup(10).unwrap();
        assert_eq!(balance.name, "BalanceError");
        assert!(balance.origin.contains("Stellar Asset Contract"));
        assert_eq!(names.name(1), None, "reserved");
        assert_eq!(names.name(16), None);
    }

    #[test]
    fn explains_why_there_are_no_names() {
        let reason = |wasm: &[u8]| from_wasm(wasm).unwrap_err().to_string();

        let no_section = wasm_with_sections(&[(1, vec![0])]);
        assert_eq!(
            reason(&no_section),
            "the contract declares no error names: the WASM has no `contractspecv0` section"
        );

        let empty = wasm_with_sections(&[(0, [&[14u8][..], b"contractspecv0"].concat())]);
        assert!(reason(&empty).contains("declares no error enum"));

        let garbage =
            wasm_with_sections(&[(0, [&[14u8][..], b"contractspecv0", &[0xff; 7]].concat())]);
        assert!(reason(&garbage).contains("section is malformed"));
    }
}
