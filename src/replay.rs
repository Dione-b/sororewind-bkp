//! replay: re-execute the transaction against the rebuilt ledger.
//!
//! This is the risky stage. Everything upstream exists to feed
//! `invoke_host_function` the exact inputs stellar-core would have handed it —
//! same host version, same footprint, same auth entries, same resource limits,
//! same cost parameters — so that the local outcome is comparable with the
//! recorded one.

use soroban_env_host::{
    budget::Budget,
    e2e_invoke::{invoke_host_function, InvokeHostFunctionResult as HostInvokeOutput},
    HostError,
};
use stellar_xdr::{DiagnosticEvent, InvokeHostFunctionResult, ScErrorType};

use crate::error::{Result, RewindError};
use crate::state::Reconstructed;
use crate::tx::DecodedTx;

/// Everything the narrative and the fidelity check need from one run.
#[derive(Debug)]
pub struct ReplayOutcome {
    /// `Ok` carries the encoded `ScVal` the host returned; `Err` is the host's
    /// own failure.
    pub invoke_result: std::result::Result<Vec<u8>, HostError>,
    pub diagnostics: Vec<DiagnosticEvent>,
    pub cpu_insns: u64,
    pub mem_bytes: u64,
    pub cpu_limit: u64,
    pub mem_limit: u64,
    /// Footprint slots that had no live entry, which is how a locally-replayed
    /// archived entry shows up.
    pub missing_entries: usize,
}

impl ReplayOutcome {
    /// Map a local failure onto the same axis stellar-core uses when it records
    /// `InvokeHostFunctionResult`.
    ///
    /// Mirrors `InvokeHostFunctionOpFrame::invokeHostFunction`: instruction
    /// over-run and memory over-run become `ResourceLimitExceeded`, an entry we
    /// could not find becomes `EntryArchived`, everything else is `Trapped`.
    pub fn local_result_code(&self) -> InvokeHostFunctionResult {
        match &self.invoke_result {
            Ok(_) => InvokeHostFunctionResult::Success(Default::default()),
            Err(err) => {
                if self.cpu_insns > self.cpu_limit || self.mem_bytes > self.mem_limit {
                    return InvokeHostFunctionResult::ResourceLimitExceeded;
                }
                let storage_error = sc_error_type(err) == Some(ScErrorType::Storage);
                if self.missing_entries > 0 && storage_error {
                    return InvokeHostFunctionResult::EntryArchived;
                }
                InvokeHostFunctionResult::Trapped
            }
        }
    }

    pub fn local_result_label(&self) -> &'static str {
        match self.local_result_code() {
            InvokeHostFunctionResult::Success(_) => "success",
            InvokeHostFunctionResult::Malformed => "malformed",
            InvokeHostFunctionResult::Trapped => "trapped",
            InvokeHostFunctionResult::ResourceLimitExceeded => "resource limit exceeded",
            InvokeHostFunctionResult::EntryArchived => "entry archived",
            InvokeHostFunctionResult::InsufficientRefundableFee => "insufficient refundable fee",
        }
    }
}

/// Does the local run agree with what the network recorded?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fidelity {
    /// Same result — the local replay reproduces the network's outcome.
    Match,
    /// Different result. The state under us has almost certainly moved.
    Diverged { onchain: String, local: String },
}

pub fn compare(onchain: &InvokeHostFunctionResult, local: &InvokeHostFunctionResult) -> Fidelity {
    // Compare the full XDR value. Matching only the result discriminant would
    // call two successful invocations a match even when their return values or
    // events differ, which overstates replay fidelity.
    if onchain == local {
        Fidelity::Match
    } else {
        Fidelity::Diverged {
            onchain: result_label(onchain),
            local: result_label(local),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{compare, Fidelity};
    use stellar_xdr::{Hash, InvokeHostFunctionResult};

    #[test]
    fn matching_failure_codes_match() {
        assert_eq!(
            compare(
                &InvokeHostFunctionResult::Trapped,
                &InvokeHostFunctionResult::Trapped
            ),
            Fidelity::Match
        );
    }

    #[test]
    fn different_result_codes_diverge() {
        assert!(matches!(
            compare(
                &InvokeHostFunctionResult::Trapped,
                &InvokeHostFunctionResult::ResourceLimitExceeded
            ),
            Fidelity::Diverged { .. }
        ));
    }

    #[test]
    fn success_payload_is_part_of_fidelity() {
        let onchain = InvokeHostFunctionResult::Success(Hash([0; 32]));
        let local = InvokeHostFunctionResult::Success(Hash([1; 32]));
        assert!(matches!(
            compare(&onchain, &local),
            Fidelity::Diverged { .. }
        ));
    }
}

/// The same words stellar-core would use for this result code.
pub fn result_label(r: &InvokeHostFunctionResult) -> String {
    match r {
        InvokeHostFunctionResult::Success(hash) => {
            format!("success (result hash {})", hex::encode(hash.0))
        }
        InvokeHostFunctionResult::Malformed => "malformed".to_string(),
        InvokeHostFunctionResult::Trapped => "trapped".to_string(),
        InvokeHostFunctionResult::ResourceLimitExceeded => "resource limit exceeded".to_string(),
        InvokeHostFunctionResult::EntryArchived => "entry archived".to_string(),
        InvokeHostFunctionResult::InsufficientRefundableFee => {
            "insufficient refundable fee".to_string()
        }
    }
}

pub fn replay(tx: &DecodedTx, state: &Reconstructed) -> Result<ReplayOutcome> {
    let encoded_host_fn = tx.encoded_host_function()?;
    let encoded_resources = tx.encoded_resources()?;
    let encoded_source_account = tx.encoded_source_account()?;
    let encoded_auth = tx.encoded_auth()?;
    let encoded_entries = state.encoded_entries()?;

    let cpu_limit = tx.resources.instructions as u64;
    let mem_limit = state.config.tx_memory_limit as u64;

    // Cost parameters come from the ledger's own ConfigSetting entries so the
    // host prices every instruction exactly as the network did. When the RPC
    // would not serve them we fall back to the host's calibrated defaults and
    // say so in the report.
    let budget = Budget::try_from_configs(
        cpu_limit,
        mem_limit,
        state.config.cpu_cost_params.clone(),
        state.config.mem_cost_params.clone(),
    )
    .map_err(|e| RewindError::Replay(format!("could not configure the execution budget: {e}")))?;

    let mut diagnostics: Vec<DiagnosticEvent> = Vec::new();

    // The network seeds the host PRNG from the transaction-set hash and the
    // transaction's position inside it, neither of which is retrievable from
    // the RPC. We use a fixed seed; a contract that draws random numbers will
    // diverge, and the fidelity check below is what surfaces that.
    const PRNG_SEED: [u8; 32] = [0u8; 32];

    let output: HostInvokeOutput = invoke_host_function(
        &budget,
        true,
        encoded_host_fn,
        encoded_resources,
        &tx.restored_rw_indices,
        encoded_source_account,
        encoded_auth.into_iter(),
        state.ledger_info.clone(),
        encoded_entries.into_iter(),
        PRNG_SEED.to_vec(),
        &mut diagnostics,
        None,
        None,
    )
    .map_err(|e| {
        RewindError::Replay(format!(
            "the host refused to start (this usually means the ledger state is inconsistent \
             with the transaction): {e}"
        ))
    })?;

    let cpu_insns = budget.get_cpu_insns_consumed().unwrap_or_default();
    let mem_bytes = budget.get_mem_bytes_consumed().unwrap_or_default();

    Ok(ReplayOutcome {
        invoke_result: output.encoded_invoke_result,
        diagnostics,
        cpu_insns,
        mem_bytes,
        cpu_limit,
        mem_limit,
        missing_entries: state.missing().len(),
    })
}

fn sc_error_type(err: &HostError) -> Option<ScErrorType> {
    let sc: ScErrorType = {
        let s: stellar_xdr::ScError = err.try_into().ok()?;
        match s {
            stellar_xdr::ScError::Contract(_) => ScErrorType::Contract,
            stellar_xdr::ScError::WasmVm(_) => ScErrorType::WasmVm,
            stellar_xdr::ScError::Context(_) => ScErrorType::Context,
            stellar_xdr::ScError::Storage(_) => ScErrorType::Storage,
            stellar_xdr::ScError::Object(_) => ScErrorType::Object,
            stellar_xdr::ScError::Crypto(_) => ScErrorType::Crypto,
            stellar_xdr::ScError::Events(_) => ScErrorType::Events,
            stellar_xdr::ScError::Budget(_) => ScErrorType::Budget,
            stellar_xdr::ScError::Value(_) => ScErrorType::Value,
            stellar_xdr::ScError::Auth(_) => ScErrorType::Auth,
        }
    };
    Some(sc)
}
