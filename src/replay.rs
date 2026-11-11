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
//! and the ones that can be evaluated offline are mirrored here:
//!
//! - *before* the host: an archived footprint entry that is not listed for
//!   restoration fails the operation with `ENTRY_ARCHIVED`;
//! - *host failure*: instructions or memory over the limit map to
//!   `RESOURCE_LIMIT_EXCEEDED`, anything else to `TRAPPED`;
//! - *after a successful host run*: bytes written beyond the declared
//!   `writeBytes`, or events plus return value beyond the network's
//!   `txMaxContractEventsSizeBytes`, are `RESOURCE_LIMIT_EXCEEDED`.
//!
//! Not mirrored: the refundable-fee check (`INSUFFICIENT_REFUNDABLE_FEE`),
//! the disk-read byte limit for classic entries, and per-entry size limits.
//! Those need fee state and settings the replay does not reconstruct; such a
//! transaction shows up as a divergence.

use serde::Serialize;
use sha2::{Digest, Sha256};
use soroban_env_host::{
    budget::Budget,
    e2e_invoke::{invoke_host_function, LedgerEntryChange},
    storage::Storage,
    vm::VersionedContractCodeCostInputs,
    Host, HostError, ModuleCache,
};
use stellar_xdr::{
    ContractCodeEntryExt, ContractEvent, ContractEventBody, ContractEventType, ContractEventV0,
    DiagnosticEvent, ExtensionPoint, Hash, InvokeHostFunctionResult,
    InvokeHostFunctionSuccessPreImage, LedgerEntryData, LedgerKey, Limits, ReadXdr, ScBytes,
    ScError, ScErrorCode, ScString, ScSymbol, ScVal, ScVec, StringM,
};

use crate::codec;
use crate::display;
use crate::error::{Result, RewindError};
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
    pub budget: BudgetUse,
    pub prng_seed: PrngSeed,
}

/// Budget consumed by the local replay, next to the limits it ran under and,
/// when the network reported them, the amounts the network consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetUse {
    pub cpu_insns: u64,
    pub cpu_limit: u64,
    pub mem_bytes: u64,
    pub mem_limit: u64,
    /// `cpu_insn` from the network's own `core_metrics` diagnostics.
    pub network_cpu_insns: Option<u64>,
    /// `mem_byte` from the network's own `core_metrics` diagnostics.
    pub network_mem_bytes: Option<u64>,
}

/// Where the host's PRNG seed came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
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
    #[serde(rename = "match")]
    pub matches: bool,
    pub divergence: Option<Divergence>,
}

/// How the local run differs from the network's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Divergence {
    /// A different `InvokeHostFunctionResult`.
    Result,
    /// The same result code, reached through an error the network never
    /// recorded: the replay failed, but not the way the transaction did.
    Error,
}

impl Fidelity {
    pub const MATCH: Fidelity = Fidelity {
        matches: true,
        divergence: None,
    };

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

pub fn replay(
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
    // up at the first archived entry.
    if let Some(slot) = state.slots.iter().find(|slot| slot.archived()) {
        let (error, event) = archived_entry_error(slot);
        return Ok(ReplayOutcome {
            result: InvokeHostFunctionResult::EntryArchived,
            error: Some(error),
            diagnostics: vec![event],
            budget: budget_use,
            prng_seed,
        });
    }

    // Cost parameters come from the ledger's own ConfigSetting entries so the
    // host prices every instruction exactly as the network did.
    let budget = Budget::try_from_configs(
        budget_use.cpu_limit,
        budget_use.mem_limit,
        state.config.cpu_cost_params.clone(),
        state.config.mem_cost_params.clone(),
    )
    .map_err(|e| RewindError::Replay(format!("could not configure the execution budget: {e}")))?;

    let mut diagnostics: Vec<DiagnosticEvent> = Vec::new();
    let output = invoke_host_function(
        &budget,
        true,
        tx.encoded_host_function()?,
        tx.encoded_resources()?,
        &tx.restored_rw_indices,
        tx.encoded_source_account()?,
        tx.encoded_auth()?.into_iter(),
        state.ledger_info.clone(),
        state.encoded_entries()?.into_iter(),
        seed.to_vec(),
        &mut diagnostics,
        None,
        Some(module_cache(state)?),
    )
    .map_err(|e| {
        RewindError::Replay(format!(
            "the host refused to start (this usually means the ledger state is inconsistent \
             with the transaction): {e}"
        ))
    })?;

    budget_use.cpu_insns = budget.get_cpu_insns_consumed().unwrap_or_default();
    budget_use.mem_bytes = budget.get_mem_bytes_consumed().unwrap_or_default();

    let (result, error) = match &output.encoded_invoke_result {
        Err(host_error) => (
            host_failure_code(&budget_use),
            ScError::try_from(host_error).ok(),
        ),
        Ok(return_value) => {
            let checks = PostHostChecks {
                write_bytes_limit: u64::from(tx.resources.write_bytes),
                events_size_limit: u64::from(state.config.tx_max_contract_events_size_bytes),
            };
            match checks.run(
                &output.ledger_changes,
                &output.encoded_contract_events,
                return_value,
            ) {
                None => (
                    InvokeHostFunctionResult::Success(success_hash(
                        return_value,
                        &output.encoded_contract_events,
                    )?),
                    None,
                ),
                Some((error, event)) => {
                    diagnostics.push(event);
                    (InvokeHostFunctionResult::ResourceLimitExceeded, Some(error))
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
}

impl PostHostChecks {
    /// The first limit the host output breaks, as stellar-core would report it.
    fn run(
        &self,
        changes: &[LedgerEntryChange],
        events: &[Vec<u8>],
        return_value: &[u8],
    ) -> Option<(ScError, DiagnosticEvent)> {
        // Every read-write entry that still exists is written back.
        let mut written = 0u64;
        for change in changes.iter().filter(|change| !change.read_only) {
            written += change
                .encoded_new_value
                .as_ref()
                .map_or(0, |v| v.len() as u64);
            if written > self.write_bytes_limit {
                return Some(core_error(
                    ScError::Budget(ScErrorCode::ExceededLimit),
                    "operation byte-write resources exceeds amount specified",
                    vec![ScVal::U64(written), ScVal::U64(self.write_bytes_limit)],
                ));
            }
        }

        let mut emitted = 0u64;
        for event in events {
            emitted += event.len() as u64;
            if emitted > self.events_size_limit {
                return Some(core_error(
                    ScError::Budget(ScErrorCode::ExceededLimit),
                    "total events size exceeds network config maximum",
                    vec![ScVal::U64(emitted), ScVal::U64(self.events_size_limit)],
                ));
            }
        }
        emitted += return_value.len() as u64;
        if emitted > self.events_size_limit {
            return Some(core_error(
                ScError::Budget(ScErrorCode::ExceededLimit),
                "return value pushes events size above network config maximum",
                vec![ScVal::U64(emitted), ScVal::U64(self.events_size_limit)],
            ));
        }
        None
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

/// Pre-parse every contract in the footprint, the way stellar-core does.
///
/// Since protocol 23 core keeps all contract code parsed in a module cache,
/// so an invocation is only charged for *instantiating* a module. Without the
/// cache the host parses each module inside the invocation and charges for it,
/// which inflates the instruction count by an order of a million per contract
/// and can push a replay over a limit the network never hit.
fn module_cache(state: &Reconstructed) -> Result<ModuleCache> {
    let setup =
        |e: HostError| RewindError::Replay(format!("could not set up the module cache: {e}"));

    // Parsing is not charged to the transaction; give it room to work.
    let budget = Budget::try_from_configs(
        u64::MAX,
        u64::MAX,
        state.config.cpu_cost_params.clone(),
        state.config.mem_cost_params.clone(),
    )
    .map_err(setup)?;
    let context = Host::with_storage_and_budget(Storage::default(), budget);
    let cache = ModuleCache::new(&context).map_err(setup)?;

    for slot in &state.slots {
        if slot.missing() || slot.archived() {
            continue;
        }
        let Some(LedgerEntryData::ContractCode(code)) = slot.entry.as_ref().map(|e| &e.data) else {
            continue;
        };
        let cost_inputs = match &code.ext {
            ContractCodeEntryExt::V0 => VersionedContractCodeCostInputs::V0 {
                wasm_bytes: code.code.len(),
            },
            ContractCodeEntryExt::V1(v1) => {
                VersionedContractCodeCostInputs::V1(v1.cost_inputs.clone())
            }
        };
        // A module that does not parse is simply not cached; the host reports
        // the problem itself if the transaction goes on to call it.
        if let Err(e) = cache.parse_and_cache_module(
            &context,
            state.ledger_info.protocol_version,
            &code.hash,
            &code.code,
            cost_inputs,
        ) {
            log::debug!(target: "replay", "module {} not cached: {e}", hex::encode(code.hash.0));
        }
    }
    Ok(cache)
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

    fn change(read_only: bool, new_value: Option<usize>) -> LedgerEntryChange {
        LedgerEntryChange {
            read_only,
            encoded_key: Vec::new(),
            old_entry_size_bytes_for_rent: 0,
            encoded_new_value: new_value.map(|n| vec![0; n]),
            new_entry_size_bytes_for_rent: 0,
            ttl_change: None,
        }
    }

    #[test]
    fn post_host_checks_mirror_core() {
        let checks = PostHostChecks {
            write_bytes_limit: 100,
            events_size_limit: 50,
        };
        // Read-only entries and removed entries are not writes.
        let changes = [
            change(true, Some(500)),
            change(false, None),
            change(false, Some(100)),
        ];
        assert!(checks.run(&changes, &[], &[]).is_none());

        let (error, event) = checks
            .run(
                &[change(false, Some(60)), change(false, Some(41))],
                &[],
                &[],
            )
            .unwrap();
        assert_eq!(error, ScError::Budget(ScErrorCode::ExceededLimit));
        assert_eq!(
            display::diagnostic(&event),
            "fail [-] error, budget error ExceededLimit \
             [\"operation byte-write resources exceeds amount specified\", 101, 100]"
        );

        assert!(checks.run(&[], &[vec![0; 30]], &[0; 20]).is_none());
        let (_, event) = checks.run(&[], &[vec![0; 30], vec![0; 21]], &[]).unwrap();
        assert!(display::diagnostic(&event).contains("total events size exceeds"));
        let (_, event) = checks.run(&[], &[vec![0; 30]], &[0; 21]).unwrap();
        assert!(display::diagnostic(&event).contains("return value pushes events size"));
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
