//! the pipeline: fetch → decode → reconstruct → replay → narrate → report.
//!
//! The order is the product: every stage exists to feed the next one exactly
//! what it needs, and the fidelity check at the end is only meaningful if
//! nothing upstream quietly substituted a guess for a fact. [`run`] keeps that
//! order on one screen; each stage is a function with a typed result.

use std::path::PathBuf;

use crate::display;
use crate::error::{Result, Retention, RewindError};
use crate::hash::TxHash;
use crate::network::{Network, NetworkId};
use crate::replay::{self, Fidelity, ReplayOutcome};
use crate::report::{
    Diagnostics, FootprintReport, Report, ResultPair, SCHEMA_VERSION, TtlDriftEntry,
};
use crate::rpc::{RawTransaction, RpcClient, TxStatus};
use crate::state::{self, LedgerContext, Reconstructed};
use crate::story::Story;
use crate::tx::{self, DecodedTx};
use crate::{host, spec, strkey};
use stellar_xdr::{ConfigSettingId, InvokeHostFunctionResult};

/// What to replay.
#[derive(Debug, Clone)]
pub struct Request {
    /// The transaction to replay.
    pub hash: TxHash,
    /// The network the caller believes the RPC serves. When set, a different
    /// passphrase is an error instead of a silently mislabelled report.
    pub network: Option<Network>,
    /// Read the failing contract's error names from this WASM instead of the
    /// ledger.
    pub wasm_debug: Option<PathBuf>,
}

/// Replay `request.hash` against `rpc` and assemble the [`Report`]: fetch,
/// decode, reconstruct the ledger state, replay, compare with what the
/// network recorded, and narrate the outcome.
pub async fn run(rpc: &RpcClient, request: &Request) -> Result<Report> {
    let fetched = fetch(rpc, request).await?;
    let tx = tx::decode(&request.hash, &fetched.raw)?;
    // The header and the entries do not depend on each other: one round trip.
    let (ledger, entries) = tokio::try_join!(
        state::read_ledger(rpc, tx.ledger, &fetched.passphrase),
        state::fetch(rpc, &tx),
    )?;
    let host = embedded_host(&tx, &ledger, &fetched.network)?;
    let state = state::reconstruct(rpc, &tx, &ledger, entries).await?;
    let outcome = replay::replay(host, &tx, &state, &ledger)?;
    let fidelity = replay::compare(&tx, &outcome);
    let story = narrate(rpc, request, &state, &outcome).await?;

    Ok(assemble(Replayed {
        rpc,
        network: fetched.network,
        tx,
        ledger,
        state,
        outcome,
        fidelity,
        story,
    }))
}

/// A failed transaction and the network it was found on.
struct Fetched {
    raw: RawTransaction,
    passphrase: String,
    network: NetworkId,
}

async fn fetch(rpc: &RpcClient, request: &Request) -> Result<Fetched> {
    let (raw, info) = tokio::try_join!(rpc.get_transaction(&request.hash), rpc.network())?;

    // The passphrase is the network's identity; `--network` is only a claim.
    let network = NetworkId::from_passphrase(&info.passphrase);
    if let Some(expected) = request.network
        && network != NetworkId::Known(expected)
    {
        return Err(RewindError::NetworkMismatch {
            expected,
            actual: network,
            origin: rpc.origin(),
        });
    }

    match raw.status {
        TxStatus::Failed => Ok(Fetched {
            raw,
            passphrase: info.passphrase,
            network,
        }),
        TxStatus::Success => Err(RewindError::NotAFailure {
            hash: request.hash.clone(),
        }),
        TxStatus::NotFound => Err(RewindError::NotFound {
            hash: request.hash.clone(),
            network,
            // Only used to word the error, so a failed probe is not an error.
            retention: match rpc.health().await.map(|health| {
                (
                    health.ledger_retention_window.filter(|window| *window > 0),
                    health.oldest_ledger,
                )
            }) {
                Ok((Some(window), oldest)) => Retention::Reported { window, oldest },
                _ => Retention::Unknown,
            },
        }),
        TxStatus::Unknown => Err(RewindError::Rpc {
            origin: rpc.origin(),
            method: "getTransaction".to_string(),
            code: None,
            detail: "the transaction has a status this build of sororewind does not know \
                     (neither SUCCESS, FAILED nor NOT_FOUND)"
                .to_string(),
        }),
    }
}

/// The embedded host for the transaction's ledger. Each host hard-rejects
/// any ledger whose protocol differs from its own, so checking here turns a
/// cryptic host error into an actionable sentence.
fn embedded_host(
    tx: &DecodedTx,
    ledger: &LedgerContext,
    network: &NetworkId,
) -> Result<&'static host::Host> {
    host::for_protocol(ledger.protocol).ok_or_else(|| RewindError::ProtocolMismatch {
        hash: tx.hash.clone(),
        tx_protocol: ledger.protocol,
        supported: host::describe(),
        network: network.clone(),
    })
}

/// Build the call tree and give the contract error behind the failure, the
/// fatal one or the one a host error wraps, its name.
async fn narrate(
    rpc: &RpcClient,
    request: &Request,
    state: &Reconstructed,
    outcome: &ReplayOutcome,
) -> Result<Story> {
    let mut story = Story::build(&outcome.diagnostics, outcome.error.as_ref());
    let Some(failure) = &story.failure else {
        return Ok(story);
    };
    // The contract the error was raised in, falling back to the outermost
    // call when the failure happened before any frame opened.
    let (code, contract) = match (&failure.cause, failure.contract_code) {
        (Some(cause), _) => (cause.contract_code, cause.contract.clone()),
        (None, Some(code)) => (code, failure.contract.clone()),
        (None, None) => return Ok(story),
    };
    let contract = contract.or_else(|| story.calls.first().and_then(|call| call.contract.clone()));

    let Some(found) = error_name(rpc, request, state, contract.as_deref(), code).await? else {
        return Ok(story);
    };
    if let Some(failure) = &mut story.failure {
        match &mut failure.cause {
            Some(cause) => {
                cause.name = Some(found.name);
                cause.name_origin = Some(found.origin);
            }
            None => {
                failure.name = Some(found.name);
                failure.name_origin = Some(found.origin);
            }
        }
    }
    Ok(story)
}

/// The name `contract` declares for error `code`, and where it was read from.
///
/// The contract's code is usually already in the footprint; when it is not, it
/// costs two extra `getLedgerEntries` calls, which is still cheap next to the
/// value of printing `InsufficientBalance` instead of `#3`.
async fn error_name(
    rpc: &RpcClient,
    request: &Request,
    state: &Reconstructed,
    contract: Option<&str>,
    code: u32,
) -> Result<Option<spec::ErrorName>> {
    let (names, source) = match &request.wasm_debug {
        Some(path) => {
            let wasm = tokio::fs::read(path)
                .await
                .map_err(|source| RewindError::WasmFile {
                    path: path.clone(),
                    source,
                })?;
            (
                spec::from_wasm(&wasm),
                format!(", from --wasm-debug {}", path.display()),
            )
        }
        None => {
            let Some(contract_id) = contract.and_then(strkey::parse_contract) else {
                return Ok(None);
            };
            match spec::contract_wasm(rpc, state, &contract_id).await {
                Ok(spec::ContractCode::StellarAsset) => {
                    (Ok(spec::stellar_asset_contract()), String::new())
                }
                // The name is read from the code the contract has now.
                Ok(spec::ContractCode::Wasm(contract)) if contract.changed_since => (
                    spec::from_wasm(&contract.wasm),
                    ", read from code that changed after the transaction: the name may not be \
                     the one this error had"
                        .to_string(),
                ),
                Ok(spec::ContractCode::Wasm(contract)) => (
                    spec::from_wasm(&contract.wasm),
                    ", from the contract's code on the ledger".to_string(),
                ),
                Err(err) => {
                    log::debug!(target: "spec", "{err}");
                    return Ok(None);
                }
            }
        }
    };
    // A contract without a readable spec still has a number; not an error.
    match names {
        Ok(names) => Ok(names.lookup(code).map(|found| spec::ErrorName {
            origin: format!("{}{source}", found.origin),
            ..found
        })),
        Err(err) => {
            log::debug!(target: "spec", "{err}");
            Ok(None)
        }
    }
}

/// Everything the stages produced, ready to be summarised.
struct Replayed<'a> {
    rpc: &'a RpcClient,
    network: NetworkId,
    tx: DecodedTx,
    ledger: LedgerContext,
    state: Reconstructed,
    outcome: ReplayOutcome,
    fidelity: Fidelity,
    story: Story,
}

fn assemble(run: Replayed<'_>) -> Report {
    Report {
        schema_version: SCHEMA_VERSION,
        hash: run.tx.hash.clone(),
        fee_bump: run.tx.fee_bump,
        network: match &run.network {
            NetworkId::Known(network) => network.name().to_string(),
            NetworkId::Custom(passphrase) => passphrase.clone(),
        },
        rpc: run.rpc.origin(),
        ledger: run.ledger.sequence,
        protocol: run.ledger.protocol,
        close_time: run.ledger.close_time,
        status: "failed",
        fidelity: run.fidelity,
        result: ResultPair {
            onchain: (&run.tx.onchain_result).into(),
            local: (&run.outcome.result).into(),
        },
        story: run.story,
        budget: run.outcome.budget,
        footprint: footprint_report(
            &run.state,
            run.ledger.sequence,
            [&run.tx.onchain_result, &run.outcome.result]
                .contains(&&InvokeHostFunctionResult::InsufficientRefundableFee),
        ),
        prng_seed: run.outcome.prng_seed,
        diagnostics: Diagnostics {
            onchain: lines(&run.tx.onchain_diagnostic_events),
            local: lines(&run.outcome.diagnostics),
            onchain_xdr: raw(&run.tx.onchain_diagnostic_events),
            local_xdr: raw(&run.outcome.diagnostics),
        },
    }
}

fn lines(events: &[stellar_xdr::DiagnosticEvent]) -> Vec<String> {
    events.iter().map(display::diagnostic).collect()
}

fn raw(events: &[stellar_xdr::DiagnosticEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| crate::codec::encode(event, "a diagnostic event").ok())
        .map(|bytes| crate::codec::b64_encode(&bytes))
        .collect()
}

fn footprint_report(state: &Reconstructed, tx_ledger: u32, fee_decided: bool) -> FootprintReport {
    let labels = |keep: &dyn Fn(&state::FootprintSlot) -> bool| -> Vec<String> {
        state
            .slots
            .iter()
            .filter(|slot| keep(slot))
            .map(|slot| display::ledger_key(&slot.key))
            .collect()
    };
    let mut config_drifted: Vec<String> = state
        .config
        .drifted
        .iter()
        .map(|setting| setting.to_string())
        .collect();
    // The state-size window only prices rent, so it is only worth a mention
    // when the refundable fee decided a result.
    if fee_decided && state.config.state_size_window_drifted {
        config_drifted.push(
            ConfigSettingId::LiveSorobanStateSizeWindow
                .name()
                .to_string(),
        );
    }
    FootprintReport {
        entries: state.slots.len(),
        missing: state.slots.iter().filter(|slot| slot.missing()).count(),
        drifted: labels(&|slot| slot.value_drifted(tx_ledger)),
        ttl_drifted: state
            .slots
            .iter()
            .filter_map(|slot| {
                Some(TtlDriftEntry {
                    entry: display::ledger_key(&slot.key),
                    reason: slot.ttl_drift?.into(),
                })
            })
            .collect(),
        archived: labels(&|slot| slot.archived()),
        archived_inferred: labels(&|slot| {
            matches!(slot.liveness, state::Liveness::Archived { inferred: true })
        }),
        rebuilt: labels(&|slot| slot.rebuilt),
        rebuild_note: state.rebuild_note.clone(),
        drift_unknown: labels(&|slot| slot.drift_unknown()),
        ext_missing: labels(&|slot| slot.ext_missing),
        config_drifted,
        config_drift_unknown: state
            .config
            .drift_unknown
            .iter()
            .map(|setting| setting.to_string())
            .collect(),
    }
}
