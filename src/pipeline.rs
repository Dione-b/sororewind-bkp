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
    Diagnostics, FootprintReport, Report, ResultPair, TtlDriftEntry, SCHEMA_VERSION,
};
use crate::rpc::{RawTransaction, RpcClient, TxStatus};
use crate::state::{self, LedgerContext, Reconstructed};
use crate::story::{Failure, Story};
use crate::tx::{self, DecodedTx};
use crate::{spec, strkey, HOST_VERSION};

/// What to replay.
#[derive(Debug, Clone)]
pub struct Request {
    pub hash: TxHash,
    /// The network the caller believes the RPC serves. When set, a different
    /// passphrase is an error instead of a silently mislabelled report.
    pub network: Option<Network>,
    /// Read the failing contract's error names from this WASM instead of the
    /// ledger.
    pub wasm_debug: Option<PathBuf>,
}

pub async fn run(rpc: &RpcClient, request: &Request) -> Result<Report> {
    let fetched = fetch(rpc, request).await?;
    let tx = tx::decode(&request.hash, &fetched.raw)?;
    let ledger = state::read_ledger(rpc, tx.ledger, &fetched.passphrase).await?;
    guard_protocol(&tx, &ledger, &fetched.network)?;
    let state = state::reconstruct(rpc, &tx, &ledger).await?;
    let outcome = replay::replay(&tx, &state, &ledger)?;
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
    if let Some(expected) = request.network {
        if network != NetworkId::Known(expected) {
            return Err(RewindError::NetworkMismatch {
                expected,
                actual: network,
                origin: rpc.origin(),
            });
        }
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
            retention: match rpc.health().await {
                Ok(health) if health.ledger_retention_window > 0 => Retention::Reported {
                    window: health.ledger_retention_window,
                    oldest: health.oldest_ledger,
                },
                _ => Retention::Unknown,
            },
        }),
    }
}

/// The host hard-rejects any ledger whose protocol differs from its own, so
/// checking here turns a cryptic host error into an actionable sentence.
fn guard_protocol(tx: &DecodedTx, ledger: &LedgerContext, network: &NetworkId) -> Result<()> {
    let host_protocol = crate::host_protocol();
    if ledger.protocol == host_protocol {
        return Ok(());
    }
    Err(RewindError::ProtocolMismatch {
        hash: tx.hash.clone(),
        tx_protocol: ledger.protocol,
        host_protocol,
        host_version: HOST_VERSION,
        network: network.clone(),
    })
}

/// Build the call tree and, for a contract error, give the number its name.
async fn narrate(
    rpc: &RpcClient,
    request: &Request,
    state: &Reconstructed,
    outcome: &ReplayOutcome,
) -> Result<Story> {
    let mut story = Story::build(&outcome.diagnostics, outcome.error.as_ref());
    let Some(code) = story.failure.as_ref().and_then(|f| f.contract_code) else {
        return Ok(story);
    };

    let wasm = match &request.wasm_debug {
        Some(path) => tokio::fs::read(path)
            .await
            .map_err(|source| RewindError::WasmFile {
                path: path.clone(),
                source,
            })?,
        None => match failing_contract_wasm(rpc, state, &story).await {
            Some(wasm) => wasm,
            None => return Ok(story),
        },
    };

    // A contract without a readable spec still has a number; not an error.
    match spec::from_wasm(&wasm) {
        Ok(names) => {
            if let Some(failure) = &mut story.failure {
                failure.name = names.name(code).map(str::to_string);
                failure.name_origin = names.origin;
            }
        }
        Err(err) => log::debug!(target: "spec", "{err}"),
    }
    Ok(story)
}

/// The WASM of the contract the error was raised in, falling back to the
/// outermost call when the failure happened before any frame opened.
///
/// The contract's code is usually already in the footprint; when it is not, it
/// costs two extra `getLedgerEntries` calls, which is still cheap next to the
/// value of printing `InsufficientBalance` instead of `#3`.
async fn failing_contract_wasm(
    rpc: &RpcClient,
    state: &Reconstructed,
    story: &Story,
) -> Option<Vec<u8>> {
    let contract = story
        .failure
        .as_ref()
        .and_then(|failure: &Failure| failure.contract.as_deref())
        .or_else(|| {
            story
                .calls
                .first()
                .and_then(|call| call.contract.as_deref())
        })?;
    let contract_id = strkey::parse_contract(contract)?;
    match spec::contract_wasm(rpc, state, &contract_id).await {
        Ok(wasm) => Some(wasm),
        Err(err) => {
            log::debug!(target: "spec", "{err}");
            None
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
        status: TxStatus::Failed,
        fidelity: run.fidelity,
        result: ResultPair {
            onchain: (&run.tx.onchain_result).into(),
            local: (&run.outcome.result).into(),
        },
        story: run.story,
        budget: run.outcome.budget,
        footprint: footprint_report(&run.state, run.ledger.sequence),
        prng_seed: run.outcome.prng_seed,
        diagnostics: Diagnostics {
            onchain: lines(&run.tx.onchain_diagnostic_events),
            local: lines(&run.outcome.diagnostics),
        },
    }
}

fn lines(events: &[stellar_xdr::DiagnosticEvent]) -> Vec<String> {
    events.iter().map(display::diagnostic).collect()
}

fn footprint_report(state: &Reconstructed, tx_ledger: u32) -> FootprintReport {
    let labels = |keep: &dyn Fn(&state::FootprintSlot) -> bool| -> Vec<String> {
        state
            .slots
            .iter()
            .filter(|slot| keep(slot))
            .map(|slot| display::ledger_key(&slot.key))
            .collect()
    };
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
        config_drifted: state
            .config
            .drifted
            .iter()
            .map(|setting| setting.to_string())
            .collect(),
    }
}
