//! spec: read a contract's own vocabulary out of its WASM.
//!
//! When a contract fails with `ScError::Contract(n)` the host only tells us a
//! number. The name lives in the contract's own `contractspecv0` section, so
//! that is where we go to turn `#3` into `InsufficientBalance`.

use std::collections::HashMap;
use std::io::Cursor;

use stellar_xdr::{
    ContractDataDurability, ContractExecutable, LedgerEntry, LedgerEntryData, LedgerKey,
    LedgerKeyContractCode, LedgerKeyContractData, Limited, Limits, ReadXdr, ScAddress, ScSpecEntry,
    ScVal, WriteXdr,
};

use crate::error::{Result, RewindError};
use crate::rpc::{self, RpcClient};
use crate::state::Reconstructed;

/// Contract-declared error names, keyed by the numeric code an
/// `ScError::Contract(n)` carries.
#[derive(Debug, Default, Clone)]
pub struct ErrorNames {
    names: HashMap<u32, String>,
    /// Where the names came from, shown under `-v` so a reader can tell a real
    /// lookup from an absent spec.
    pub origin: Option<String>,
}

impl ErrorNames {
    pub fn name(&self, code: u32) -> Option<&str> {
        self.names.get(&code).map(String::as_str)
    }
}

/// Read the error enum out of a WASM's `contractspecv0` custom section.
///
/// The section is a bare stream of `ScSpecEntry` XDR values with no framing,
/// so it is decoded entry by entry until the bytes run out.
pub fn from_wasm(wasm: &[u8]) -> Result<ErrorNames> {
    let section = custom_section(wasm, "contractspecv0").ok_or_else(|| {
        RewindError::Decode(
            "the WASM has no `contractspecv0` section, so it declares no errors".into(),
        )
    })?;

    let mut reader = Limited::new(
        Cursor::new(section),
        Limits {
            depth: 500,
            len: 0x100_0000,
        },
    );

    let mut names = HashMap::new();
    let mut enum_name = None;
    for entry in ScSpecEntry::read_xdr_iter(&mut reader) {
        let entry = entry.map_err(|e| {
            RewindError::Decode(format!("the `contractspecv0` section is malformed: {e}"))
        })?;
        if let ScSpecEntry::UdtErrorEnumV0(err_enum) = entry {
            enum_name = Some(err_enum.name.to_string_lossy_like());
            for case in err_enum.cases.iter() {
                names.insert(case.value, case.name.to_string_lossy_like());
            }
        }
    }

    if names.is_empty() {
        return Err(RewindError::Decode(
            "the `contractspecv0` section declares no error enum".into(),
        ));
    }

    Ok(ErrorNames {
        names,
        origin: enum_name.map(|n| format!("spec enum {n}")),
    })
}

/// Convenience: turn a `StringM` (arbitrary bytes, usually UTF-8) into text.
trait StringMLike {
    fn to_string_lossy_like(&self) -> String;
}

impl<const MAX: u32> StringMLike for stellar_xdr::StringM<MAX> {
    fn to_string_lossy_like(&self) -> String {
        String::from_utf8_lossy(self).into_owned()
    }
}

/// Locate the `contractspecv0` custom section in a WASM binary.
///
/// Hand-rolled rather than pulled from `wasmparser`: the binary format is a
/// flat sequence of `id + leb128 size + payload`, and this only ever needs to
/// find one custom section.
fn custom_section<'a>(wasm: &'a [u8], wanted: &str) -> Option<&'a [u8]> {
    if wasm.len() < 8 || &wasm[..4] != b"\0asm" {
        return None;
    }
    let mut i = 8usize;
    while i < wasm.len() {
        let id = wasm[i];
        i += 1;
        let (size, n) = leb128(&wasm[i..])?;
        i += n;
        let end = i.checked_add(size)?;
        if end > wasm.len() {
            return None;
        }
        if id == 0 {
            let (name_len, n2) = leb128(&wasm[i..])?;
            let name_start = i + n2;
            let name_end = name_start.checked_add(name_len)?;
            if name_end <= end && &wasm[name_start..name_end] == wanted.as_bytes() {
                return Some(&wasm[name_end..end]);
            }
        }
        i = end;
    }
    None
}

fn leb128(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut value = 0usize;
    let mut shift = 0u32;
    for (n, byte) in bytes.iter().take(5).enumerate() {
        value |= ((*byte as usize) & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((value, n + 1));
        }
        shift += 7;
    }
    None
}

/// Fetch the WASM behind a contract: its instance entry tells us the code
/// hash, the code entry gives us the bytes. Both are usually already in the
/// footprint; when they are not, they are one `getLedgerEntries` away.
pub async fn contract_wasm(
    rpc: &RpcClient,
    state: &Reconstructed,
    contract_id: &stellar_xdr::ContractId,
) -> Result<Vec<u8>> {
    let instance_key = LedgerKey::ContractData(LedgerKeyContractData {
        contract: ScAddress::Contract(contract_id.clone()),
        key: ScVal::LedgerKeyContractInstance,
        durability: ContractDataDurability::Persistent,
    });
    let instance = fetch_entry(rpc, state, &instance_key).await?;

    let LedgerEntryData::ContractData(data) = instance.data else {
        return Err(RewindError::State(
            "the contract's instance entry is not contract data".into(),
        ));
    };
    let ScVal::ContractInstance(inst) = data.val else {
        return Err(RewindError::State(
            "the contract's instance entry does not hold a contract instance".into(),
        ));
    };
    let ContractExecutable::Wasm(hash) = inst.executable else {
        return Err(RewindError::State(
            "this contract has no WASM of its own (it is the native asset contract), \
             so it declares no error names"
                .into(),
        ));
    };

    let code_key = LedgerKey::ContractCode(LedgerKeyContractCode { hash });
    let code = fetch_entry(rpc, state, &code_key).await?;
    let LedgerEntryData::ContractCode(code) = code.data else {
        return Err(RewindError::State(
            "the contract's code entry does not hold contract code".into(),
        ));
    };
    Ok(code.code.as_slice().to_vec())
}

/// Look a ledger entry up in the reconstructed footprint first, and only go to
/// the network when the transaction never touched it.
async fn fetch_entry(
    rpc: &RpcClient,
    state: &Reconstructed,
    key: &LedgerKey,
) -> Result<LedgerEntry> {
    let key_bytes = key
        .to_xdr(Limits::none())
        .map_err(|e| RewindError::State(format!("encoding ledger key: {e}")))?;

    if let Some(slot) = state.slots.iter().find(|s| s.key_xdr == key_bytes) {
        return slot
            .entry
            .clone()
            .ok_or_else(|| RewindError::State("the entry is no longer on the ledger".into()));
    }

    let b64 = rpc::b64_encode(&key_bytes);
    let returned = rpc.get_ledger_entries(std::slice::from_ref(&b64)).await?;
    let mut ordered = rpc::order_entries(std::slice::from_ref(&b64), returned);
    let raw = ordered.pop().flatten().ok_or_else(|| {
        RewindError::State(
            "the RPC has no record of the contract's code; it may have expired from \
             the ledger's retention window"
                .into(),
        )
    })?;

    rpc::assemble_entry(&raw)
}
