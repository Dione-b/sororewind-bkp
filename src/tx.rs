//! decode: turn the raw RPC payload into the exact inputs the host needs.
//!
//! Nothing here talks to the network. The stage answers one question — *is
//! this a Soroban transaction, and what exactly did it ask the host to do?* —
//! and fails loudly when the answer is no.

use stellar_xdr::{
    AccountId, ContractEvent, ContractEventBody, ContractEventV0, DiagnosticEvent, HostFunction,
    InvokeHostFunctionResult, Limits, OperationBody, OperationResult, OperationResultTr, ReadXdr,
    ScVal, SorobanAuthorizationEntry, SorobanResources, Transaction, TransactionEnvelope,
    TransactionExt, TransactionMeta, TransactionResult, TransactionResultResult, WriteXdr,
};

use crate::error::{Result, RewindError};
use crate::rpc::RawTransaction;

/// Everything replay needs, lifted out of the envelope.
#[derive(Debug)]
pub struct DecodedTx {
    pub hash: String,
    pub ledger: u32,
    pub status: String,

    pub source_account: AccountId,
    pub host_function: HostFunction,
    pub resources: SorobanResources,
    pub auth: Vec<SorobanAuthorizationEntry>,
    /// Indices into `resources.footprint.read_write` of entries the transaction
    /// asked the host to auto-restore. Empty for ordinary invocations.
    pub restored_rw_indices: Vec<u32>,

    /// What the network actually recorded.
    pub onchain_result: InvokeHostFunctionResult,
    pub onchain_diagnostic_events: Vec<DiagnosticEvent>,
}

impl DecodedTx {
    /// Footprint keys in the order the host demands: read-only first, then
    /// read-write. `invoke_host_function` is positional about this.
    pub fn footprint_keys(&self) -> Vec<stellar_xdr::LedgerKey> {
        self.resources
            .footprint
            .read_only
            .iter()
            .chain(self.resources.footprint.read_write.iter())
            .cloned()
            .collect()
    }

    pub fn encoded_host_function(&self) -> Result<Vec<u8>> {
        encode(&self.host_function, "host function")
    }

    pub fn encoded_resources(&self) -> Result<Vec<u8>> {
        encode(&self.resources, "resources")
    }

    pub fn encoded_source_account(&self) -> Result<Vec<u8>> {
        encode(&self.source_account, "source account")
    }

    pub fn encoded_auth(&self) -> Result<Vec<Vec<u8>>> {
        self.auth.iter().map(|a| encode(a, "auth entry")).collect()
    }
}

pub fn decode(hash: &str, raw: &RawTransaction) -> Result<DecodedTx> {
    let not_soroban = |reason: &str| RewindError::NotSoroban {
        hash: hash.to_string(),
        reason: reason.to_string(),
    };

    let envelope_xdr = raw
        .envelope_xdr
        .as_deref()
        .ok_or_else(|| not_soroban("the RPC response contained no envelopeXdr"))?;

    let envelope_bytes = crate::rpc::b64_decode(envelope_xdr, "envelopeXdr")?;
    let envelope = TransactionEnvelope::from_xdr(&envelope_bytes, Limits::none())
        .map_err(|e| RewindError::Decode(format!("envelopeXdr: {e}")))?;

    let tx: Transaction = match envelope {
        TransactionEnvelope::Tx(v1) => v1.tx,
        TransactionEnvelope::TxV0(_) => {
            return Err(not_soroban(
                "it is a legacy v0 envelope, which cannot carry Soroban resources or a footprint",
            ));
        }
        TransactionEnvelope::TxFeeBump(_) => {
            return Err(not_soroban(
                "it is a fee-bump transaction; Soroban transactions cannot be fee-bumped, \
                 so this envelope is malformed",
            ));
        }
    };

    let soroban_data = match tx.ext {
        TransactionExt::V1(data) => data,
        TransactionExt::V0 => {
            return Err(not_soroban(
                "its `tx.ext` is empty, so it carries no Soroban resources or footprint",
            ));
        }
    };

    if tx.operations.len() != 1 {
        return Err(not_soroban(&format!(
            "it has {} operations; Soroban transactions must have exactly one",
            tx.operations.len()
        )));
    }

    let op = tx.operations.first().expect("length checked above").clone();

    let invoke = match op.body {
        OperationBody::InvokeHostFunction(f) => f,
        other => {
            return Err(not_soroban(&format!(
                "its operation is `{}`, not `InvokeHostFunction`",
                operation_name(&other)
            )));
        }
    };

    let ledger = raw.ledger.ok_or_else(|| {
        RewindError::Decode("the getTransaction response had no `ledger` field".to_string())
    })?;

    let result_xdr = raw
        .result_xdr
        .as_deref()
        .ok_or_else(|| not_soroban("the RPC response contained no resultXdr"))?;
    let result_bytes = crate::rpc::b64_decode(result_xdr, "resultXdr")?;
    let tx_result = TransactionResult::from_xdr(&result_bytes, Limits::none())
        .map_err(|e| RewindError::Decode(format!("resultXdr: {e}")))?;

    let onchain_result = extract_invoke_result(&tx_result, hash)?;

    let onchain_diagnostic_events = match raw.result_meta_xdr.as_deref() {
        Some(meta_b64) => {
            let meta_bytes = crate::rpc::b64_decode(meta_b64, "resultMetaXdr")?;
            let meta = TransactionMeta::from_xdr(&meta_bytes, Limits::none())
                .map_err(|e| RewindError::Decode(format!("resultMetaXdr: {e}")))?;
            extract_soroban_meta(&meta)
        }
        None => Vec::new(),
    };

    let restored_rw_indices = match &soroban_data.ext {
        stellar_xdr::SorobanTransactionDataExt::V1(v0) => v0.archived_soroban_entries.to_vec(),
        stellar_xdr::SorobanTransactionDataExt::V0 => Vec::new(),
    };

    // The envelope carries a `MuxedAccount`, but the host wants a plain
    // `AccountId`; a muxed source would have been rejected on-chain anyway.
    let source_account = match tx.source_account {
        stellar_xdr::MuxedAccount::Ed25519(k) => {
            AccountId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(k))
        }
        stellar_xdr::MuxedAccount::MuxedEd25519(m) => {
            AccountId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(m.ed25519))
        }
    };

    Ok(DecodedTx {
        hash: hash.to_string(),
        ledger,
        status: raw.status.clone(),
        source_account,
        host_function: invoke.host_function,
        resources: soroban_data.resources,
        auth: invoke.auth.to_vec(),
        restored_rw_indices,
        onchain_result,
        onchain_diagnostic_events,
    })
}

/// Pull the `InvokeHostFunctionResult` out of the operation results.
fn extract_invoke_result(
    result: &TransactionResult,
    hash: &str,
) -> Result<InvokeHostFunctionResult> {
    let ops: Vec<OperationResult> = match &result.result {
        TransactionResultResult::TxSuccess(ops) => ops.to_vec(),
        TransactionResultResult::TxFailed(ops) => ops.to_vec(),
        other => {
            return Err(RewindError::NotAFailure {
                hash: hash.to_string(),
                status: result_code_name(other).to_string(),
            })
        }
    };

    let first = ops.first().cloned().ok_or_else(|| {
        RewindError::Decode("the transaction result contained no operation results".to_string())
    })?;

    match first {
        OperationResult::OpInner(tr) => match tr {
            OperationResultTr::InvokeHostFunction(r) => Ok(r),
            other => Err(RewindError::NotSoroban {
                hash: hash.to_string(),
                reason: format!(
                    "the recorded operation result is `{}`, not a host function result",
                    operation_result_name(&other)
                ),
            }),
        },
        other => Err(RewindError::NotSoroban {
            hash: hash.to_string(),
            reason: format!(
                "the recorded operation result is `{}`",
                operation_result_name2(&other)
            ),
        }),
    }
}

fn extract_soroban_meta(meta: &TransactionMeta) -> Vec<DiagnosticEvent> {
    match meta {
        TransactionMeta::V3(v3) => match &v3.soroban_meta {
            Some(sm) => sm.diagnostic_events.to_vec(),
            None => Vec::new(),
        },
        TransactionMeta::V4(v4) => v4.diagnostic_events.to_vec(),
        _ => Vec::new(),
    }
}

fn encode<T: WriteXdr>(value: &T, what: &str) -> Result<Vec<u8>> {
    value
        .to_xdr(Limits::none())
        .map_err(|e| RewindError::Decode(format!("could not encode {what}: {e}")))
}

pub fn result_code_name(code: &TransactionResultResult) -> &'static str {
    match code {
        TransactionResultResult::TxSuccess(_) => "tx success",
        TransactionResultResult::TxFailed(_) => "tx failed",
        TransactionResultResult::TxTooEarly => "tx too early",
        TransactionResultResult::TxTooLate => "tx too late",
        TransactionResultResult::TxMissingOperation => "missing operation",
        TransactionResultResult::TxBadSeq => "bad sequence number",
        TransactionResultResult::TxBadAuth => "bad auth",
        TransactionResultResult::TxInsufficientBalance => "insufficient balance",
        TransactionResultResult::TxNoAccount => "no such account",
        TransactionResultResult::TxInsufficientFee => "insufficient fee",
        TransactionResultResult::TxBadAuthExtra => "bad auth extra",
        TransactionResultResult::TxInternalError => "internal error",
        TransactionResultResult::TxNotSupported => "not supported",
        TransactionResultResult::TxMalformed => "malformed",
        TransactionResultResult::TxSorobanInvalid => "invalid Soroban transaction",
        TransactionResultResult::TxFrozenKeyAccessed => "frozen ledger key accessed",
        TransactionResultResult::TxBadSponsorship => "bad sponsorship",
        TransactionResultResult::TxBadMinSeqAgeOrGap => "bad min seq age or gap",
        TransactionResultResult::TxFeeBumpInnerSuccess(_) => "inner tx success",
        TransactionResultResult::TxFeeBumpInnerFailed(_) => "inner tx failed",
    }
}

fn operation_name(body: &OperationBody) -> &'static str {
    match body {
        OperationBody::InvokeHostFunction(_) => "InvokeHostFunction",
        OperationBody::ExtendFootprintTtl(_) => "ExtendFootprintTtl",
        OperationBody::RestoreFootprint(_) => "RestoreFootprint",
        OperationBody::Payment(_) => "Payment",
        OperationBody::CreateAccount(_) => "CreateAccount",
        OperationBody::ManageData(_) => "ManageData",
        OperationBody::ChangeTrust(_) => "ChangeTrust",
        OperationBody::SetOptions(_) => "SetOptions",
        OperationBody::AccountMerge(_) => "AccountMerge",
        OperationBody::BumpSequence(_) => "BumpSequence",
        OperationBody::Inflation => "Inflation",
        _ => "another classic operation",
    }
}

fn operation_result_name(tr: &OperationResultTr) -> &'static str {
    match tr {
        OperationResultTr::InvokeHostFunction(_) => "InvokeHostFunction",
        OperationResultTr::ExtendFootprintTtl(_) => "ExtendFootprintTtl",
        OperationResultTr::RestoreFootprint(_) => "RestoreFootprint",
        _ => "another classic operation result",
    }
}

fn operation_result_name2(r: &OperationResult) -> &'static str {
    match r {
        OperationResult::OpInner(_) => "inner operation",
        OperationResult::OpBadAuth => "bad auth",
        OperationResult::OpNoAccount => "no account",
        OperationResult::OpNotSupported => "not supported",
        OperationResult::OpTooManySubentries => "too many subentries",
        OperationResult::OpExceededWorkLimit => "exceeded work limit",
        OperationResult::OpTooManySponsoring => "too many sponsoring",
    }
}

/// Diagnostic events, as they come back from the RPC, are already in the same
/// shape the host produces locally — so they can be compared one-for-one.
pub fn diagnostic_summary(events: &[DiagnosticEvent]) -> Vec<String> {
    events.iter().map(diagnostic_one).collect()
}

fn diagnostic_one(d: &DiagnosticEvent) -> String {
    let contract = d
        .event
        .contract_id
        .as_ref()
        .map(|c| crate::strkey::contract(&c.0 .0))
        .unwrap_or_else(|| "-".to_string());

    let (topics, data) = split_event(&d.event);

    let topics: Vec<String> = topics.iter().map(scval_short).collect();
    format!(
        "{} [{}] {} {}",
        if d.in_successful_contract_call {
            "ok"
        } else {
            "fail"
        },
        contract,
        topics.join(", "),
        scval_short(&data)
    )
}

pub fn split_event(event: &ContractEvent) -> (Vec<ScVal>, ScVal) {
    match &event.body {
        ContractEventBody::V0(ContractEventV0 { topics, data }) => (topics.to_vec(), data.clone()),
    }
}

/// A one-line rendering of an `ScVal`, used for both the narrative and the
/// on-chain/local diagnostic comparison.
pub fn scval_short(v: &ScVal) -> String {
    let mut out = String::new();
    if let Err(e) = write_scval(v, &mut out, 0) {
        return format!("<unprintable: {e}>");
    }
    out
}

fn write_scval(v: &ScVal, out: &mut String, depth: usize) -> std::fmt::Result {
    use std::fmt::Write;
    if depth > 4 {
        return write!(out, "…");
    }
    match v {
        ScVal::Void => write!(out, "void"),
        ScVal::Bool(b) => write!(out, "{b}"),
        ScVal::U32(n) => write!(out, "{n}"),
        ScVal::I32(n) => write!(out, "{n}"),
        ScVal::U64(n) => write!(out, "{n}"),
        ScVal::I64(n) => write!(out, "{n}"),
        ScVal::Timepoint(t) => write!(out, "{}", t.0),
        ScVal::Duration(d) => write!(out, "{}s", d.0),
        ScVal::U128(p) => write!(out, "{}", ((p.hi as u128) << 64) | p.lo as u128),
        ScVal::I128(p) => write!(
            out,
            "{}",
            ((p.hi as i128) << 64) | ((p.lo as u128) as i128 & 0xFFFF_FFFF_FFFF_FFFF)
        ),
        ScVal::U256(p) => write!(
            out,
            "{:016x}{:016x}{:016x}{:016x}",
            p.hi_hi, p.hi_lo, p.lo_hi, p.lo_lo
        ),
        ScVal::I256(p) => write!(
            out,
            "{:016x}{:016x}{:016x}{:016x}",
            p.hi_hi, p.hi_lo, p.lo_hi, p.lo_lo
        ),
        ScVal::Bytes(b) => {
            if b.0.len() <= 16 {
                write!(out, "0x{}", hex::encode(&b.0))
            } else {
                write!(out, "0x{}…({} bytes)", hex::encode(&b.0[..8]), b.0.len())
            }
        }
        // `StringM`'s `Display` already escapes quotes and backslashes, so
        // wrapping it in quotes yields a printable string literal.
        ScVal::String(s) => write!(out, "\"{}\"", s.0),
        ScVal::Symbol(s) => write!(out, "{}", s.0),
        ScVal::Error(e) => write!(out, "{}", sc_error_display(e)),
        ScVal::Address(a) => write!(out, "{}", crate::strkey::address(a)),
        ScVal::ContractInstance(_) => write!(out, "<contract instance>"),
        ScVal::LedgerKeyContractInstance => write!(out, "<contract instance key>"),
        ScVal::LedgerKeyNonce(n) => write!(out, "<nonce {}>", n.nonce),
        ScVal::ExecutableTag(s) => write!(out, "<executable {}>", s.0),
        ScVal::Vec(items) => {
            write!(out, "[")?;
            let empty: Vec<ScVal> = Vec::new();
            let items = items.as_ref().map(|v| &v.0[..]).unwrap_or(empty.as_slice());
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    write!(out, ", ")?;
                }
                write_scval(item, out, depth + 1)?;
            }
            write!(out, "]")
        }
        ScVal::Map(entries) => {
            write!(out, "{{")?;
            let empty: Vec<stellar_xdr::ScMapEntry> = Vec::new();
            let entries = entries
                .as_ref()
                .map(|m| &m.0[..])
                .unwrap_or(empty.as_slice());
            for (i, e) in entries.iter().enumerate() {
                if i > 0 {
                    write!(out, ", ")?;
                }
                write_scval(&e.key, out, depth + 1)?;
                write!(out, ": ")?;
                write_scval(&e.val, out, depth + 1)?;
            }
            write!(out, "}}")
        }
    }
}

fn sc_error_display(e: &stellar_xdr::ScError) -> String {
    match e {
        stellar_xdr::ScError::Contract(n) => format!("contract error #{n}"),
        stellar_xdr::ScError::WasmVm(c) => format!("wasm vm: {}", code_name(c)),
        stellar_xdr::ScError::Context(c) => format!("context: {}", code_name(c)),
        stellar_xdr::ScError::Storage(c) => format!("storage: {}", code_name(c)),
        stellar_xdr::ScError::Object(c) => format!("object: {}", code_name(c)),
        stellar_xdr::ScError::Crypto(c) => format!("crypto: {}", code_name(c)),
        stellar_xdr::ScError::Events(c) => format!("events: {}", code_name(c)),
        stellar_xdr::ScError::Budget(c) => format!("budget: {}", code_name(c)),
        stellar_xdr::ScError::Value(c) => format!("value: {}", code_name(c)),
        stellar_xdr::ScError::Auth(c) => format!("auth: {}", code_name(c)),
    }
}

fn code_name(c: &stellar_xdr::ScErrorCode) -> &'static str {
    use stellar_xdr::ScErrorCode::*;
    match c {
        ArithDomain => "arithmetic domain error",
        IndexBounds => "index out of bounds",
        InvalidInput => "invalid input",
        MissingValue => "missing value",
        ExistingValue => "value already exists",
        ExceededLimit => "limit exceeded",
        InvalidAction => "invalid action",
        InternalError => "internal error",
        UnexpectedType => "unexpected type",
        UnexpectedSize => "unexpected size",
    }
}
