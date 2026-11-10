//! the pipeline: fetch → decode → reconstruct → replay → narrate → render.
//!
//! One function, because the order is the product: every stage exists to feed
//! the next one exactly what it needs, and the fidelity check at the end is
//! only meaningful if nothing upstream quietly substituted a guess for a fact.

use stellar_strkey::Contract as ContractStrkey;
use stellar_xdr::ScError;

use crate::cli::Cli;
use crate::error::{Result, RewindError};
use crate::render::{self, BudgetReport, DriftReport, Report};
use crate::replay::{self, Fidelity};
use crate::rpc::RpcClient;
use crate::state;
use crate::story::Story;
use crate::{spec, strkey, tx};

/// Everything went fine and the local replay reproduced the network.
pub const EXIT_OK: i32 = 0;
/// The tool itself failed: bad input, unreachable RPC, un-replayable state.
pub const EXIT_ERROR: i32 = 1;
/// The replay ran but disagreed with the network — state has drifted.
pub const EXIT_DIVERGED: i32 = 2;

/// Version of the embedded `soroban-env-host`, filled in by `build.rs` from
/// `Cargo.lock` so the protocol-guard message can name it accurately.
const HOST_VERSION: &str = env!("SOROBAN_ENV_HOST_VERSION");

pub async fn run(cli: &Cli) -> Result<i32> {
    let hash = normalize_hash(&cli.tx_hash)?;
    let rpc = RpcClient::new(
        cli.network,
        cli.rpc_url.clone(),
        cli.verbose,
        cli.fixture.clone(),
        cli.record.clone(),
    );

    // --- fetch ------------------------------------------------------------
    let (response, network) = tokio::try_join!(rpc.get_transaction(&hash), rpc.network_info())?;
    let raw = response.tx;

    if raw.status == "NOT_FOUND" {
        // Only used to word the error; a failed health probe still leaves a
        // sensible retention window behind.
        let health = rpc.health().await.ok();
        return Err(RewindError::NotFound {
            hash,
            network: cli.network.name().to_string(),
            window: health
                .as_ref()
                .map(|h| h.ledger_retention_window)
                .unwrap_or(120_960),
            oldest: health.as_ref().map(|h| h.oldest_ledger).unwrap_or(0),
        });
    }
    if raw.status != "FAILED" {
        return Err(RewindError::NotAFailure {
            hash,
            status: raw.status.clone(),
        });
    }

    // --- decode -----------------------------------------------------------
    let tx = tx::decode(&hash, &raw)?;

    // --- protocol guard ---------------------------------------------------
    // The host hard-rejects any ledger whose protocol differs from its own, so
    // checking here turns a cryptic host error into an actionable sentence.
    let (header, close_time) = state::read_ledger_header(&rpc, tx.ledger).await?;
    let host_protocol = soroban_env_host::meta::INTERFACE_VERSION.protocol;
    if header.ledger_version != host_protocol {
        return Err(RewindError::ProtocolMismatch {
            hash,
            tx_protocol: header.ledger_version,
            host_protocol,
            host_version: HOST_VERSION,
            wanted: host_protocol,
            network: cli.network.name().to_string(),
        });
    }

    // --- reconstruct ------------------------------------------------------
    let footprint = tx.footprint_keys();
    let state = state::reconstruct(
        &rpc,
        &footprint,
        tx.ledger,
        &network.passphrase,
        header.ledger_version,
        header.base_reserve,
        close_time,
    )
    .await?;

    // --- replay -----------------------------------------------------------
    let outcome = replay::replay(&tx, &state)?;
    let local_result = outcome.local_result_code();
    let fidelity = replay::compare(&tx.onchain_result, &local_result);

    // --- narrate ----------------------------------------------------------
    let host_error: Option<&soroban_env_host::HostError> = outcome.invoke_result.as_ref().err();
    let sc_error: Option<ScError> = host_error.and_then(|e| e.try_into().ok());
    let story = Story::build(&outcome.diagnostics, sc_error.as_ref());
    let (error_name, error_origin) = resolve_error_names(cli, &rpc, &state, &story).await?;

    // --- render -----------------------------------------------------------
    let drifted: Vec<String> = state
        .drifted(tx.ledger)
        .iter()
        .map(|&i| strkey::ledger_key(&state.slots[i].key))
        .collect();

    let report = Report {
        hash: tx.hash.clone(),
        network: cli.network.name().to_string(),
        rpc_url: rpc.url(),
        ledger: tx.ledger,
        protocol: header.ledger_version,
        close_time,
        status: tx.status.clone(),
        fidelity: fidelity.clone(),
        onchain_label: replay::result_label(&tx.onchain_result),
        local_label: outcome.local_result_label().to_string(),
        story,
        error_name,
        error_origin,
        budget: BudgetReport {
            cpu_insns: outcome.cpu_insns,
            cpu_limit: outcome.cpu_limit,
            mem_bytes: outcome.mem_bytes,
            mem_limit: outcome.mem_limit,
        },
        drift: DriftReport {
            drifted,
            missing: state.missing().len(),
            total: state.slots.len(),
            config_from_ledger: state.config.from_ledger,
        },
        onchain_diagnostics: tx::diagnostic_summary(&tx.onchain_diagnostic_events),
        local_diagnostics: tx::diagnostic_summary(&outcome.diagnostics),
    };

    if cli.json {
        println!("{}", render::json(&report));
    } else {
        print!(
            "{}",
            render::human(&report, render::use_color(cli.no_color), cli.verbose)
        );
    }

    Ok(match fidelity {
        Fidelity::Match => EXIT_OK,
        Fidelity::Diverged { .. } => EXIT_DIVERGED,
    })
}

/// Give the failing `ScError::Contract(n)` its own name, if the contract is
/// willing to tell us one.
///
/// The contract's WASM is usually already in the footprint; when it is not, it
/// costs two extra `getLedgerEntries` calls, which is still cheap next to the
/// value of printing `InsufficientBalance` instead of `#3`.
async fn resolve_error_names(
    cli: &Cli,
    rpc: &RpcClient,
    state: &state::Reconstructed,
    story: &Story,
) -> Result<(Option<String>, Option<String>)> {
    let Some(failure) = &story.failure else {
        return Ok((None, None));
    };
    let Some(code) = failure.contract_error_code() else {
        return Ok((None, None));
    };

    let wasm = if let Some(path) = &cli.wasm_debug {
        std::fs::read(path).map_err(|e| {
            RewindError::State(format!(
                "could not read --wasm-debug {}: {e}",
                path.display()
            ))
        })?
    } else {
        let Some(contract) = failing_contract(story) else {
            return Ok((None, None));
        };
        let Some(contract_id) = decode_contract(&contract) else {
            return Ok((None, None));
        };
        match spec::contract_wasm(rpc, state, &contract_id).await {
            Ok(wasm) => wasm,
            Err(err) => {
                if cli.verbose > 0 {
                    eprintln!("[spec] {err}");
                }
                return Ok((None, None));
            }
        }
    };

    match spec::from_wasm(&wasm) {
        Ok(names) => Ok((names.name(code).map(str::to_string), names.origin)),
        Err(err) => {
            if cli.verbose > 0 {
                eprintln!("[spec] {err}");
            }
            Ok((None, None))
        }
    }
}

/// The contract the error was raised in, falling back to the outermost call
/// when the failure happened before any frame opened.
fn failing_contract(story: &Story) -> Option<String> {
    story
        .failure
        .as_ref()
        .and_then(|f| f.contract.clone())
        .or_else(|| story.calls.first().and_then(|c| c.contract.clone()))
}

fn decode_contract(strkey: &str) -> Option<stellar_xdr::ContractId> {
    let contract = ContractStrkey::from_string(strkey).ok()?;
    Some(stellar_xdr::ContractId(stellar_xdr::Hash(contract.0)))
}

/// Accept the hash the way a person would paste it: optional `0x`, any case,
/// but nothing else.
fn normalize_hash(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let bare = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);

    if bare.len() != 64 || !bare.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(RewindError::InvalidHash {
            hash: raw.to_string(),
            reason: "expected 64 hexadecimal characters (32 bytes)".to_string(),
        });
    }
    // The RPC is case-sensitive about hashes and always reports them lowercase.
    Ok(bare.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_hex_with_prefix() {
        let hash = "a".repeat(64);
        assert_eq!(normalize_hash(&hash).unwrap(), hash);
        assert_eq!(normalize_hash(&format!("0x{hash}")).unwrap(), hash);
        assert_eq!(
            normalize_hash(&format!("0X{}", hash.to_uppercase())).unwrap(),
            hash
        );
    }

    #[test]
    fn rejects_nonsense() {
        assert!(normalize_hash("").is_err());
        assert!(normalize_hash("deadbeef").is_err());
        assert!(normalize_hash(&"z".repeat(64)).is_err());
        assert!(normalize_hash(&"a".repeat(65)).is_err());
    }
}
