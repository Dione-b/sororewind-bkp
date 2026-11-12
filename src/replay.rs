//! replay: re-execute the transaction against the rebuilt ledger.
//!
//! This is the risky stage. Everything upstream exists to feed
//! `invoke_host_function` the exact inputs stellar-core would have handed it —
//! same host version, same footprint, same auth entries, same resource limits,
//! same cost parameters, same module cache, same PRNG seed — so that the local
//! outcome is comparable with the recorded one.
//!
//! The host is only part of what decides an `InvokeHostFunctionResult`.
//! stellar-core's `InvokeHostFunctionOpFrame` wraps it in checks of its own,
//! which are mirrored here in core's order (checked 2026-11-11 against
//! stellar-core's `InvokeHostFunctionOpFrame.cpp`, `MutableTransactionResult.cpp`
//! and `TransactionFrame.cpp`; protocols 29 and 30 do not differ in any of
//! them, so nothing is gated on the protocol yet):
//!
//! - *before* the host, per footprint entry in order:
//!   - an archived entry that is not listed for restoration fails the
//!     operation with `ENTRY_ARCHIVED`;
//!   - contract code or data larger than the network's per-entry limit is
//!     `RESOURCE_LIMIT_EXCEEDED`;
//!   - classic entries and restored entries are read from disk, and their
//!     bytes beyond the declared `diskReadBytes` are `RESOURCE_LIMIT_EXCEEDED`
//!     (live Soroban entries are in memory since protocol 23, and free);
//! - *host failure*, during the invocation or while the host decodes its
//!   inputs: instructions or memory over the limit map to
//!   `RESOURCE_LIMIT_EXCEEDED`, anything else to `TRAPPED`;
//! - *after a successful host run*:
//!   - a written entry over the per-entry limit, bytes written beyond the
//!     declared `writeBytes`, or events plus return value beyond the
//!     network's `txMaxContractEventsSizeBytes`, are `RESOURCE_LIMIT_EXCEEDED`;
//!   - rent, then rent plus the events fee, beyond the refundable part of the
//!     declared `resourceFee` is `INSUFFICIENT_REFUNDABLE_FEE`. The fees are
//!     computed by the host's own `fees` module from the network's settings.
//!
//! The rent fee is priced from the network's state-size window, which is
//! resampled every few minutes; the replay uses the current window. Its
//! average moves slowly, so this only matters for a transaction within a few
//! stroops of its refundable fee, and the report flags the window as drifted
//! whenever that fee decided either result.

use serde::Serialize;
use sha2::{Digest, Sha256};
use stellar_xdr::{
    ContractCodeEntryExt, ContractEvent, ContractEventBody, ContractEventType, ContractEventV0,
    DiagnosticEvent, ExtensionPoint, Hash, InvokeHostFunctionResult,
    InvokeHostFunctionSuccessPreImage, LedgerEntryData, LedgerKey, Limits, ReadXdr, ScBytes,
    ScError, ScErrorCode, ScString, ScSymbol, ScVal, ScVec, StringM,
};

use crate::codec;
use crate::display;
use crate::error::{Result, RewindError};
use crate::host::{self, Change, ContractCode, FeeRequest, Fees, Invocation};
use crate::state::{FootprintSlot, LedgerContext, Reconstructed};
use crate::tx::DecodedTx;

/// Everything the narrative and the fidelity check need from one run.
#[derive(Debug)]
pub struct ReplayOutcome {
    /// The result code stellar-core would record for this run.
    pub result: InvokeHostFunctionResult,
    /// The error behind a failed result.
    pub error: Option<ScError>,
    /// The host's diagnostics, followed by the ones stellar-core would add.
    pub diagnostics: Vec<DiagnosticEvent>,
    /// CPU and memory the replay consumed, and the limits it ran under.
    pub budget: BudgetUse,
    /// Whether the PRNG seed was the network's or a zeroed stand-in.
    pub prng_seed: PrngSeed,
}

/// Budget consumed by the local replay, next to the limits it ran under and,
/// when the network reported them, the amounts the network consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetUse {
    /// CPU instructions the local replay consumed.
    pub cpu_insns: u64,
    /// The transaction's declared `instructions` resource limit.
    pub cpu_limit: u64,
    /// Memory the local replay consumed, in bytes.
    pub mem_bytes: u64,
    /// The network's per-transaction memory limit, in bytes.
    pub mem_limit: u64,
    /// `cpu_insn` from the network's own `core_metrics` diagnostics.
    pub network_cpu_insns: Option<u64>,
    /// `mem_byte` from the network's own `core_metrics` diagnostics.
    pub network_mem_bytes: Option<u64>,
}

/// Where the host's PRNG seed came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrngSeed {
    /// Derived exactly as the network derives it.
    Derived,
    /// All zeroes: the RPC did not report the transaction's apply order. A
    /// contract that draws random numbers will diverge.
    Fixed,
}

/// The seed stellar-core hands the host for one Soroban transaction.
///
/// From `LedgerManagerImpl::applyTransactions` and `subSha256` (stellar-core,
/// checked 2026-11-10 against protocol 30):
///
/// ```text
/// base = txSet.getContentsHash()            // == header.scpValue.txSetHash
/// seed = SHA-256(base || uint64_be(index))  // index: zero-based apply order
/// ```
///
/// `index` counts every transaction in the ledger, classic ones included, in
/// the order they are applied — which is the order of `LedgerCloseMeta`'s
/// `txProcessing`, and the RPC reports it one-based as `applicationOrder`.
/// Both inputs were verified against recorded `LedgerCloseMeta`: the header's
/// `txSetHash` equals SHA-256 of the generalized transaction set, and
/// `applicationOrder - 1` equals the transaction's `txProcessing` position.
pub fn prng_seed(tx_set_hash: &[u8; 32], application_order: Option<u32>) -> ([u8; 32], PrngSeed) {
    let Some(index) = application_order.and_then(|order| order.checked_sub(1)) else {
        return ([0; 32], PrngSeed::Fixed);
    };
    let mut hasher = Sha256::new();
    hasher.update(tx_set_hash);
    hasher.update(u64::from(index).to_be_bytes());
    (hasher.finalize().into(), PrngSeed::Derived)
}

/// Does the local run agree with what the network recorded?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Fidelity {
    /// The local run reproduced the network's outcome; see [`Divergence`] for
    /// what is compared.
    #[serde(rename = "match")]
    pub matches: bool,
    /// How the run differs; `None` when it matches.
    pub divergence: Option<Divergence>,
}

/// How the local run differs from the network's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Divergence {
    /// A different `InvokeHostFunctionResult`.
    Result,
    /// The same result code, reached through an error the network never
    /// recorded: the replay failed, but not the way the transaction did.
    Error,
}

impl Fidelity {
    /// The local run reproduced the network's outcome.
    pub const MATCH: Fidelity = Fidelity {
        matches: true,
        divergence: None,
    };

    /// The local run differed from the network's, in the given way.
    pub const fn diverged(divergence: Divergence) -> Fidelity {
        Fidelity {
            matches: false,
            divergence: Some(divergence),
        }
    }
}

/// Compare the local outcome with the network's.
///
/// The full result value is compared, not just its discriminant: two
/// successful invocations with different return values or events are not a
/// match. Result codes are coarse, though — almost every contract failure is
/// `TRAPPED` — so when the network kept its diagnostics, the local error must
/// also be one the network recorded.
pub fn compare(tx: &DecodedTx, outcome: &ReplayOutcome) -> Fidelity {
    if tx.onchain_result != outcome.result {
        return Fidelity::diverged(Divergence::Result);
    }
    let onchain_errors: Vec<&ScError> = tx
        .onchain_diagnostic_events
        .iter()
        .filter_map(error_of)
        .collect();
    match &outcome.error {
        Some(local) if !onchain_errors.is_empty() && !onchain_errors.contains(&local) => {
            Fidelity::diverged(Divergence::Error)
        }
        _ => Fidelity::MATCH,
    }
}

/// The `ScError` an `error` diagnostic carries.
fn error_of(event: &DiagnosticEvent) -> Option<&ScError> {
    match display::event_parts(&event.event).0 {
        [ScVal::Symbol(kind), ScVal::Error(error), ..] if kind.0.as_slice() == b"error" => {
            Some(error)
        }
        _ => None,
    }
}

/// Re-execute `tx` with `host`, the embedded host for the ledger's protocol.
pub fn replay(
    host: &host::Host,
    tx: &DecodedTx,
    state: &Reconstructed,
    ledger: &LedgerContext,
) -> Result<ReplayOutcome> {
    let (seed, prng_seed) = prng_seed(&ledger.tx_set_hash, tx.application_order);
    let mut budget_use = BudgetUse {
        cpu_insns: 0,
        cpu_limit: u64::from(tx.resources.instructions),
        mem_bytes: 0,
        mem_limit: u64::from(state.config.tx_memory_limit),
        network_cpu_insns: core_metric(&tx.onchain_diagnostic_events, "cpu_insn"),
        network_mem_bytes: core_metric(&tx.onchain_diagnostic_events, "mem_byte"),
    };

    // stellar-core loads the footprint before it invokes the host, and gives
    // up at the first entry it cannot take.
    if let Some((result, error, event)) = footprint_checks(tx, state) {
        return Ok(ReplayOutcome {
            result,
            error: Some(error),
            diagnostics: vec![event],
            budget: budget_use,
            prng_seed,
        });
    }

    let restored_rw_indices = state.restored_rw_indices();
    let invocation = Invocation {
        host_function: tx.encoded_host_function()?,
        resources: tx.encoded_resources()?,
        source_account: tx.encoded_source_account()?,
        auth: tx.encoded_auth()?,
        restored_rw_indices: &restored_rw_indices,
        ledger: state.ledger_info.clone(),
        entries: state.encoded_entries(),
        prng_seed: seed,
        cpu_limit: budget_use.cpu_limit,
        mem_limit: budget_use.mem_limit,
        cpu_cost_params: codec::encode(&state.config.cpu_cost_params, "the CPU cost parameters")?,
        mem_cost_params: codec::encode(
            &state.config.mem_cost_params,
            "the memory cost parameters",
        )?,
        contracts: contracts(state)?,
    };
    let output = host.invoke(&invocation).map_err(|failure| match failure {
        host::Failure::Xdr(e) => RewindError::Replay(format!(
            "could not hand the transaction to soroban-env-host {}: {e}",
            host.version
        )),
        host::Failure::Budget(e) => {
            RewindError::Replay(format!("could not configure the execution budget: {e}"))
        }
        host::Failure::ModuleCache(e) => {
            RewindError::Replay(format!("could not set up the module cache: {e}"))
        }
        host::Failure::Start(e) => RewindError::Replay(format!(
            "the host failed with an internal error, which the network would not have recorded \
             as a result (this usually means the ledger state is inconsistent with the \
             transaction): {e}"
        )),
        host::Failure::Rent(index) => RewindError::Replay(format!(
            "could not size {} for rent",
            state.slots.get(index).map_or_else(
                || format!("footprint entry {index}"),
                |slot| display::ledger_key(&slot.key)
            )
        )),
    })?;

    budget_use.cpu_insns = output.cpu_insns;
    budget_use.mem_bytes = output.mem_bytes;
    let mut diagnostics = output
        .diagnostics
        .iter()
        .map(|event| from_host::<DiagnosticEvent>(event, "a diagnostic event"))
        .collect::<Result<Vec<_>>>()?;

    let (result, error) = match &output.result {
        Err(host_error) => (
            host_failure_code(&budget_use),
            host_error
                .as_deref()
                .map(|error| from_host::<ScError>(error, "the host error"))
                .transpose()?,
        ),
        Ok(return_value) => {
            let checks = PostHostChecks {
                write_bytes_limit: u64::from(tx.resources.write_bytes),
                events_size_limit: u64::from(state.config.tx_max_contract_events_size_bytes),
                sizes: SizeLimits::of(state),
            };
            let failed = match checks.run(&output.changes, &output.contract_events, return_value) {
                Err(failed) => Some(failed),
                Ok(events_size) => {
                    let request = fee_request(tx, state, ledger, &output.changes, events_size);
                    refundable_fee_check(
                        tx.resource_fee,
                        // Before apply, core prices the declared resources
                        // with no events at all.
                        &host.fees(&FeeRequest {
                            contract_events_size_bytes: 0,
                            ..request.clone()
                        }),
                        &host.fees(&request),
                    )
                }
            };
            match failed {
                None => (
                    InvokeHostFunctionResult::Success(success_hash(
                        return_value,
                        &output.contract_events,
                    )?),
                    None,
                ),
                Some((result, error, event)) => {
                    diagnostics.push(event);
                    (result, Some(error))
                }
            }
        }
    };

    Ok(ReplayOutcome {
        result,
        error,
        diagnostics,
        budget: budget_use,
        prng_seed,
    })
}

/// A check stellar-core makes around the host, failed: the result it records
/// and the diagnostic it emits.
type Failed = (InvokeHostFunctionResult, ScError, DiagnosticEvent);

fn limit_exceeded(message: &str, used: u64, limit: u64) -> Failed {
    let (error, event) = core_error(
        ScError::Budget(ScErrorCode::ExceededLimit),
        message,
        vec![ScVal::U64(used), ScVal::U64(limit)],
    );
    (
        InvokeHostFunctionResult::ResourceLimitExceeded,
        error,
        event,
    )
}

/// The network's per-entry size limits (`validateContractLedgerEntry`).
#[derive(Debug, Clone, Copy)]
struct SizeLimits {
    contract_code: u64,
    contract_data: u64,
}

impl SizeLimits {
    fn of(state: &Reconstructed) -> Self {
        Self {
            contract_code: u64::from(state.config.max_contract_size_bytes),
            contract_data: u64::from(state.config.max_contract_data_entry_size_bytes),
        }
    }

    fn check(&self, entry: &LedgerEntryData, size: u64) -> Option<Failed> {
        match entry {
            LedgerEntryData::ContractCode(_) if size > self.contract_code => Some(limit_exceeded(
                "Wasm size exceeds network config maximum contract size",
                size,
                self.contract_code,
            )),
            LedgerEntryData::ContractData(_) if size > self.contract_data => Some(limit_exceeded(
                "ContractData size exceeds network config maximum size",
                size,
                self.contract_data,
            )),
            _ => None,
        }
    }
}

/// stellar-core's `addReads`: walk the footprint in order and fail at the
/// first entry that is archived, too large, or pushes the bytes read from
/// disk over what the transaction declared.
fn footprint_checks(tx: &DecodedTx, state: &Reconstructed) -> Option<Failed> {
    let sizes = SizeLimits::of(state);
    let disk_read_limit = u64::from(tx.resources.disk_read_bytes);
    let mut disk_read = 0u64;

    for slot in &state.slots {
        if slot.archived() {
            let (error, event) = archived_entry_error(slot);
            return Some((InvokeHostFunctionResult::EntryArchived, error, event));
        }
        let (Some(entry), Some(xdr)) = (&slot.entry, &slot.entry_xdr) else {
            continue;
        };
        if slot.missing() {
            continue;
        }
        let size = xdr.len() as u64;
        if let Some(failed) = sizes.check(&entry.data, size) {
            return Some(failed);
        }
        // Since protocol 23 live Soroban entries are served from memory; only
        // classic entries and restored ones are disk reads.
        let soroban = matches!(
            slot.key,
            LedgerKey::ContractData(_) | LedgerKey::ContractCode(_)
        );
        if !soroban || slot.restored() {
            disk_read += size;
            if disk_read > disk_read_limit {
                return Some(limit_exceeded(
                    "operation byte-read resources exceeds amount specified",
                    disk_read,
                    disk_read_limit,
                ));
            }
        }
    }
    None
}

/// What the transaction declared and what the run produced, as the fee
/// computation wants it (`TransactionFrame::computeSorobanResourceFee`).
fn fee_request(
    tx: &DecodedTx,
    state: &Reconstructed,
    ledger: &LedgerContext,
    changes: &[Change],
    events_size: u64,
) -> FeeRequest {
    let footprint = &tx.resources.footprint;
    let classic = footprint
        .read_only
        .iter()
        .chain(footprint.read_write.iter())
        .filter(|key| !matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_)))
        .count();
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    FeeRequest {
        instructions: tx.resources.instructions,
        // Classic entries, plus every entry listed for restoration: archived
        // entries are the only Soroban entries read from disk.
        disk_read_entries: count(classic + tx.restored_rw_indices.len()),
        write_entries: count(footprint.read_write.len()),
        disk_read_bytes: tx.resources.disk_read_bytes,
        write_bytes: tx.resources.write_bytes,
        contract_events_size_bytes: u32::try_from(events_size).unwrap_or(u32::MAX),
        transaction_size_bytes: tx.envelope_size,
        rent_changes: changes.iter().filter_map(|change| change.rent).collect(),
        ledger_sequence: ledger.sequence,
        settings: state.config.fees,
    }
}

/// `RefundableFeeTracker::consumeRefundableSorobanResources`: the refundable
/// part of the declared resource fee must cover rent, and then rent plus the
/// events fee.
fn refundable_fee_check(declared: i64, pre_apply: &Fees, consumed: &Fees) -> Option<Failed> {
    let insufficient = |message: &str, needed: i64, available: i64| {
        // core hands both amounts to `makeU64SCVal`.
        let (error, event) = core_error(
            ScError::Budget(ScErrorCode::ExceededLimit),
            message,
            vec![ScVal::U64(needed as u64), ScVal::U64(available as u64)],
        );
        Some((
            InvokeHostFunctionResult::InsufficientRefundableFee,
            error,
            event,
        ))
    };
    let refundable = declared.saturating_sub(pre_apply.non_refundable);
    if refundable < consumed.rent {
        return insufficient(
            "refundable resource fee was not sufficient to cover the ledger storage rent: {} > {}",
            consumed.rent,
            refundable,
        );
    }
    if refundable < consumed.rent.saturating_add(consumed.refundable) {
        return insufficient(
            "refundable resource fee was not sufficient to cover the events fee after paying \
             for ledger storage rent: {} > {}",
            consumed.refundable,
            refundable - consumed.rent,
        );
    }
    None
}

/// How stellar-core classifies a failed host run.
fn host_failure_code(budget: &BudgetUse) -> InvokeHostFunctionResult {
    if budget.cpu_insns > budget.cpu_limit || budget.mem_bytes > budget.mem_limit {
        InvokeHostFunctionResult::ResourceLimitExceeded
    } else {
        InvokeHostFunctionResult::Trapped
    }
}

/// The limits stellar-core enforces on the output of a successful host run.
struct PostHostChecks {
    write_bytes_limit: u64,
    events_size_limit: u64,
    sizes: SizeLimits,
}

impl PostHostChecks {
    /// The first limit the host output breaks, as stellar-core would report
    /// it; otherwise the size of the events and the return value, which the
    /// events fee is charged on.
    #[allow(clippy::result_large_err)] // a failed check is as likely as a passed one
    fn run(
        &self,
        changes: &[Change],
        events: &[Vec<u8>],
        return_value: &[u8],
    ) -> std::result::Result<u64, Failed> {
        // Every read-write entry that still exists is written back.
        let mut written = 0u64;
        for change in changes.iter().filter(|change| !change.read_only) {
            let Some(value) = &change.new_value else {
                continue;
            };
            let size = value.len() as u64;
            // The limits depend only on the kind of entry, which leads its XDR.
            if let Ok(entry) = stellar_xdr::LedgerEntry::from_xdr(value, Limits::none())
                && let Some(failed) = self.sizes.check(&entry.data, size)
            {
                return Err(failed);
            }
            written += size;
            if written > self.write_bytes_limit {
                return Err(limit_exceeded(
                    "operation byte-write resources exceeds amount specified",
                    written,
                    self.write_bytes_limit,
                ));
            }
        }

        let mut emitted = 0u64;
        for event in events {
            emitted += event.len() as u64;
            if emitted > self.events_size_limit {
                return Err(limit_exceeded(
                    "total events size exceeds network config maximum",
                    emitted,
                    self.events_size_limit,
                ));
            }
        }
        emitted += return_value.len() as u64;
        if emitted > self.events_size_limit {
            return Err(limit_exceeded(
                "return value pushes events size above network config maximum",
                emitted,
                self.events_size_limit,
            ));
        }
        Ok(emitted)
    }
}

/// The error stellar-core raises for an archived footprint entry that the
/// transaction did not list for restoration.
fn archived_entry_error(slot: &FootprintSlot) -> (ScError, DiagnosticEvent) {
    let (message, args) = match &slot.key {
        LedgerKey::ContractCode(code) => (
            "trying to access an archived contract code entry",
            vec![ScVal::Bytes(ScBytes(
                code.hash.0.to_vec().try_into().unwrap_or_default(),
            ))],
        ),
        LedgerKey::ContractData(data) => (
            "trying to access an archived contract data entry",
            vec![ScVal::Address(data.contract.clone()), data.key.clone()],
        ),
        _ => ("trying to access an archived entry", Vec::new()),
    };
    core_error(ScError::Value(ScErrorCode::InvalidInput), message, args)
}

/// An `error` diagnostic in the shape stellar-core's `pushError` emits, so a
/// locally mirrored check reads exactly like the network's own.
fn core_error(error: ScError, message: &str, args: Vec<ScVal>) -> (ScError, DiagnosticEvent) {
    let symbol = ScSymbol(StringM::try_from("error").unwrap_or_default());
    let mut data = vec![ScVal::String(ScString(
        StringM::try_from(message).unwrap_or_default(),
    ))];
    data.extend(args);
    let event = DiagnosticEvent {
        in_successful_contract_call: false,
        event: ContractEvent {
            ext: ExtensionPoint::V0,
            contract_id: None,
            type_: ContractEventType::Diagnostic,
            body: ContractEventBody::V0(ContractEventV0 {
                topics: vec![ScVal::Symbol(symbol), ScVal::Error(error.clone())]
                    .try_into()
                    .unwrap_or_default(),
                data: ScVal::Vec(Some(ScVec(data.try_into().unwrap_or_default()))),
            }),
        },
    };
    (error, event)
}

/// The hash stellar-core records for a successful invocation: SHA-256 of the
/// return value and the contract events.
fn success_hash(return_value: &[u8], events: &[Vec<u8>]) -> Result<Hash> {
    let malformed = |e: stellar_xdr::Error| {
        RewindError::Replay(format!("the host returned malformed XDR: {e}"))
    };
    let preimage = InvokeHostFunctionSuccessPreImage {
        return_value: ScVal::from_xdr(return_value, Limits::none()).map_err(malformed)?,
        events: events
            .iter()
            .map(|event| ContractEvent::from_xdr(event, Limits::none()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(malformed)?
            .try_into()
            .map_err(malformed)?,
    };
    let bytes = codec::encode(&preimage, "the success preimage")?;
    Ok(Hash(Sha256::digest(bytes).into()))
}

/// The contracts in the footprint, for the host's module cache.
///
/// Core's cache holds the code that is *live*: an archived contract the
/// transaction restores is not in it, so the host parses that module inside
/// the invocation and charges the transaction for it.
fn contracts(state: &Reconstructed) -> Result<Vec<ContractCode<'_>>> {
    state
        .slots
        .iter()
        .filter(|slot| !slot.missing() && !slot.archived() && !slot.restored())
        .filter_map(|slot| match slot.entry.as_ref().map(|e| &e.data) {
            Some(LedgerEntryData::ContractCode(code)) => Some(code),
            _ => None,
        })
        .map(|code| {
            Ok(ContractCode {
                hash: code.hash.0,
                wasm: code.code.as_slice(),
                cost_inputs: match &code.ext {
                    ContractCodeEntryExt::V0 => None,
                    ContractCodeEntryExt::V1(v1) => Some(codec::encode(
                        &v1.cost_inputs,
                        "the contract code cost inputs",
                    )?),
                },
            })
        })
        .collect()
}

/// Decode a value the host produced in its own XDR. XDR only grows, so the
/// newest `stellar-xdr` reads what any embedded host writes.
fn from_host<T: ReadXdr>(bytes: &[u8], what: &'static str) -> Result<T> {
    T::from_xdr(bytes, Limits::none())
        .map_err(|e| RewindError::Replay(format!("could not read {what} the host returned: {e}")))
}

/// A value from the `core_metrics` diagnostics stellar-core appends to every
/// Soroban transaction.
fn core_metric(events: &[DiagnosticEvent], name: &str) -> Option<u64> {
    events
        .iter()
        .find_map(|event| match display::event_parts(&event.event) {
            ([ScVal::Symbol(kind), ScVal::Symbol(metric)], ScVal::U64(value))
                if kind.0.as_slice() == b"core_metrics"
                    && metric.0.as_slice() == name.as_bytes() =>
            {
                Some(*value)
            }
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{diag, error_event, sym};

    fn tx_with(
        onchain_result: InvokeHostFunctionResult,
        events: Vec<DiagnosticEvent>,
    ) -> DecodedTx {
        let mut tx = crate::test_support::decoded_tx();
        tx.onchain_result = onchain_result;
        tx.onchain_diagnostic_events = events;
        tx
    }

    fn outcome(result: InvokeHostFunctionResult, error: Option<ScError>) -> ReplayOutcome {
        ReplayOutcome {
            result,
            error,
            diagnostics: Vec::new(),
            budget: BudgetUse {
                cpu_insns: 0,
                cpu_limit: 0,
                mem_bytes: 0,
                mem_limit: 0,
                network_cpu_insns: None,
                network_mem_bytes: None,
            },
            prng_seed: PrngSeed::Fixed,
        }
    }

    const AUTH: ScError = ScError::Auth(ScErrorCode::InvalidAction);

    #[test]
    fn matching_failure_codes_match() {
        let tx = tx_with(InvokeHostFunctionResult::Trapped, Vec::new());
        let local = outcome(InvokeHostFunctionResult::Trapped, Some(AUTH));
        assert_eq!(compare(&tx, &local), Fidelity::MATCH);
    }

    #[test]
    fn different_result_codes_diverge() {
        let tx = tx_with(InvokeHostFunctionResult::Trapped, Vec::new());
        let local = outcome(InvokeHostFunctionResult::ResourceLimitExceeded, Some(AUTH));
        assert_eq!(compare(&tx, &local), Fidelity::diverged(Divergence::Result));
    }

    #[test]
    fn success_payload_is_part_of_fidelity() {
        let tx = tx_with(InvokeHostFunctionResult::Success(Hash([0; 32])), Vec::new());
        let local = outcome(InvokeHostFunctionResult::Success(Hash([1; 32])), None);
        assert_eq!(compare(&tx, &local), Fidelity::diverged(Divergence::Result));
    }

    #[test]
    fn the_local_error_must_be_one_the_network_recorded() {
        let recorded = vec![
            error_event(Some([1; 32]), ScError::Contract(9), "caught by try_call"),
            error_event(Some([1; 32]), ScError::Contract(3), "fatal"),
        ];
        let tx = tx_with(InvokeHostFunctionResult::Trapped, recorded);

        let same = outcome(
            InvokeHostFunctionResult::Trapped,
            Some(ScError::Contract(3)),
        );
        assert_eq!(compare(&tx, &same), Fidelity::MATCH);

        // Same result code, but the replay died of something else entirely.
        let other = outcome(
            InvokeHostFunctionResult::Trapped,
            Some(ScError::Contract(4)),
        );
        assert_eq!(compare(&tx, &other), Fidelity::diverged(Divergence::Error));
    }

    #[test]
    fn prng_seed_follows_core() {
        let base = [7u8; 32];
        // applicationOrder is one-based; core hashes the zero-based index.
        let (seed, kind) = prng_seed(&base, Some(1));
        let mut expected = Sha256::new();
        expected.update(base);
        expected.update([0u8; 8]);
        assert_eq!(seed, <[u8; 32]>::from(expected.finalize()));
        assert_eq!(kind, PrngSeed::Derived);

        let (ninth, _) = prng_seed(&base, Some(9));
        let mut expected = Sha256::new();
        expected.update(base);
        expected.update([0, 0, 0, 0, 0, 0, 0, 8]);
        assert_eq!(ninth, <[u8; 32]>::from(expected.finalize()));
        assert_ne!(seed, ninth);

        assert_eq!(prng_seed(&base, None), ([0; 32], PrngSeed::Fixed));
        assert_eq!(prng_seed(&base, Some(0)), ([0; 32], PrngSeed::Fixed));
    }

    #[test]
    fn host_failures_are_classified_like_core() {
        let budget = |cpu_insns, mem_bytes| BudgetUse {
            cpu_insns,
            cpu_limit: 100,
            mem_bytes,
            mem_limit: 100,
            network_cpu_insns: None,
            network_mem_bytes: None,
        };
        use InvokeHostFunctionResult as R;
        assert_eq!(host_failure_code(&budget(100, 100)), R::Trapped);
        assert_eq!(host_failure_code(&budget(101, 0)), R::ResourceLimitExceeded);
        assert_eq!(host_failure_code(&budget(0, 101)), R::ResourceLimitExceeded);
    }

    fn change(read_only: bool, new_value: Option<usize>) -> Change {
        Change {
            read_only,
            new_value: new_value.map(|n| vec![0; n]),
            rent: None,
        }
    }

    const NO_SIZE_LIMITS: SizeLimits = SizeLimits {
        contract_code: u64::MAX,
        contract_data: u64::MAX,
    };

    fn message(failed: &Failed) -> String {
        display::diagnostic(&failed.2)
    }

    #[test]
    fn post_host_checks_mirror_core() {
        let checks = PostHostChecks {
            write_bytes_limit: 100,
            events_size_limit: 50,
            sizes: NO_SIZE_LIMITS,
        };
        // Read-only entries and removed entries are not writes.
        let changes = [
            change(true, Some(500)),
            change(false, None),
            change(false, Some(100)),
        ];
        assert_eq!(checks.run(&changes, &[], &[]).unwrap(), 0);

        let failed = checks
            .run(
                &[change(false, Some(60)), change(false, Some(41))],
                &[],
                &[],
            )
            .unwrap_err();
        assert_eq!(failed.0, InvokeHostFunctionResult::ResourceLimitExceeded);
        assert_eq!(failed.1, ScError::Budget(ScErrorCode::ExceededLimit));
        assert_eq!(
            message(&failed),
            "fail [-] error, budget error ExceededLimit \
             [\"operation byte-write resources exceeds amount specified\", 101, 100]"
        );

        // The events fee is charged on the events and the return value.
        assert_eq!(checks.run(&[], &[vec![0; 30]], &[0; 20]).unwrap(), 50);
        let failed = checks
            .run(&[], &[vec![0; 30], vec![0; 21]], &[])
            .unwrap_err();
        assert!(message(&failed).contains("total events size exceeds"));
        let failed = checks.run(&[], &[vec![0; 30]], &[0; 21]).unwrap_err();
        assert!(message(&failed).contains("return value pushes events size"));
    }

    #[test]
    fn written_entries_respect_the_per_entry_limits() {
        use crate::test_support::{code_entry, data_entry};
        use stellar_xdr::ContractDataDurability::Persistent;

        let written = |entry: &stellar_xdr::LedgerEntry| Change {
            read_only: false,
            new_value: Some(codec::encode(entry, "entry").unwrap()),
            rent: None,
        };
        let data = written(&data_entry([2; 32], Persistent, 1));
        let code = written(&code_entry(&[0; 64], 1));
        let (data_size, code_size) = (
            data.new_value.as_ref().unwrap().len() as u64,
            code.new_value.as_ref().unwrap().len() as u64,
        );
        let checks = |contract_code, contract_data| PostHostChecks {
            write_bytes_limit: u64::MAX,
            events_size_limit: u64::MAX,
            sizes: SizeLimits {
                contract_code,
                contract_data,
            },
        };
        let both = [data.clone(), code.clone()];

        // Exactly at the limit is fine.
        assert!(checks(code_size, data_size).run(&both, &[], &[]).is_ok());
        let failed = checks(code_size, data_size - 1)
            .run(&both, &[], &[])
            .unwrap_err();
        assert!(
            message(&failed).contains("ContractData size exceeds network config maximum size"),
            "{}",
            message(&failed)
        );
        let failed = checks(code_size - 1, data_size)
            .run(&both, &[], &[])
            .unwrap_err();
        assert!(
            message(&failed).contains("Wasm size exceeds network config maximum contract size")
        );
        assert_eq!(failed.0, InvokeHostFunctionResult::ResourceLimitExceeded);
    }

    #[test]
    fn the_refundable_fee_must_cover_rent_and_then_events() {
        let fees = |non_refundable, refundable, rent| Fees {
            non_refundable,
            refundable,
            rent,
        };
        // Declared 1000, of which 400 is not refundable: 600 to spend.
        let pre_apply = fees(400, 0, 0);
        assert!(refundable_fee_check(1_000, &pre_apply, &fees(400, 100, 500)).is_none());

        let failed = refundable_fee_check(1_000, &pre_apply, &fees(400, 0, 601)).unwrap();
        assert_eq!(
            failed.0,
            InvokeHostFunctionResult::InsufficientRefundableFee
        );
        assert_eq!(failed.1, ScError::Budget(ScErrorCode::ExceededLimit));
        assert_eq!(
            message(&failed),
            "fail [-] error, budget error ExceededLimit [\"refundable resource fee was not \
             sufficient to cover the ledger storage rent: {} > {}\", 601, 600]"
        );

        // Rent fits, the events fee on top of it does not.
        let failed = refundable_fee_check(1_000, &pre_apply, &fees(400, 101, 500)).unwrap();
        assert_eq!(
            message(&failed),
            "fail [-] error, budget error ExceededLimit [\"refundable resource fee was not \
             sufficient to cover the events fee after paying for ledger storage rent: {} > {}\", \
             101, 100]"
        );
    }

    #[test]
    fn every_embedded_host_prices_fees() {
        use crate::host::{FeeSettings, RentChange};

        let request = FeeRequest {
            instructions: 1_000_000,
            disk_read_entries: 1,
            write_entries: 2,
            disk_read_bytes: 200,
            write_bytes: 300,
            contract_events_size_bytes: 1_024,
            transaction_size_bytes: 500,
            rent_changes: vec![RentChange {
                is_persistent: true,
                is_code_entry: false,
                old_size_bytes: 0,
                new_size_bytes: 1_024,
                old_live_until_ledger: 0,
                new_live_until_ledger: 100_000,
            }],
            ledger_sequence: 1,
            settings: FeeSettings {
                fee_per_instruction_increment: 25,
                fee_per_disk_read_entry: 6_250,
                fee_per_write_entry: 10_000,
                fee_per_disk_read_1kb: 1_786,
                fee_per_write_1kb: 3_500,
                fee_per_historical_1kb: 16_235,
                fee_per_contract_event_1kb: 10_000,
                fee_per_transaction_size_1kb: 1_624,
                state_target_size_bytes: 3_000_000_000,
                rent_fee_1kb_state_size_low: -17_000,
                rent_fee_1kb_state_size_high: 10_000,
                state_size_rent_fee_growth_factor: 5_000,
                average_state_size_bytes: 2_500_000_000,
                persistent_rent_rate_denominator: 1_215,
                temporary_rent_rate_denominator: 2_430,
            },
        };
        for host in &host::HOSTS {
            let fees = host.fees(&request);
            // The events fee is the only refundable part of the resource fee.
            assert_eq!(fees.refundable, 10_000, "{host:?}");
            assert!(fees.non_refundable > 0 && fees.rent > 0, "{host:?}");
            let without_events = host.fees(&FeeRequest {
                contract_events_size_bytes: 0,
                ..request.clone()
            });
            assert_eq!(without_events.refundable, 0);
            assert_eq!(without_events.non_refundable, fees.non_refundable);
            assert_eq!(without_events.rent, fees.rent);
        }
    }

    #[test]
    fn reads_core_metrics() {
        let events = vec![
            diag(
                None,
                vec![sym("core_metrics"), sym("mem_byte")],
                ScVal::U64(5),
                false,
            ),
            diag(
                None,
                vec![sym("core_metrics"), sym("cpu_insn")],
                ScVal::U64(629_271),
                false,
            ),
        ];
        assert_eq!(core_metric(&events, "cpu_insn"), Some(629_271));
        assert_eq!(core_metric(&events, "mem_byte"), Some(5));
        assert_eq!(core_metric(&events, "emit_event"), None);
    }

    #[test]
    fn success_hash_covers_value_and_events() {
        let void = codec::encode(&ScVal::Void, "v").unwrap();
        let unit = codec::encode(&ScVal::U32(1), "v").unwrap();
        let a = success_hash(&void, &[]).unwrap();
        assert_ne!(a, success_hash(&unit, &[]).unwrap());
        assert_eq!(a, success_hash(&void, &[]).unwrap());
        assert!(success_hash(&[1, 2, 3], &[]).is_err());
    }
}
