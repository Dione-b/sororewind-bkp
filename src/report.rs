//! The outcome of a run, as data.
//!
//! [`Report`] holds structured values — result codes, error categories,
//! counts — and leaves wording to the renderers. Serialising it *is* the
//! `--json` document, so the struct is the schema: a field added here shows up
//! in the output, and [`SCHEMA_VERSION`] says when the shape changed
//! incompatibly.

use serde::Serialize;
use stellar_xdr::InvokeHostFunctionResult;

use crate::display;
use crate::hash::TxHash;
use crate::replay::{BudgetUse, Fidelity, PrngSeed};
use crate::state::TtlDrift;
use crate::story::Story;

/// Version of the JSON document. Bumped when a field is removed, renamed or
/// changes meaning; adding fields does not bump it.
pub const SCHEMA_VERSION: u32 = 1;

/// Everything both renderers need.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// Always [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The transaction that was replayed.
    pub hash: TxHash,
    /// The transaction was wrapped in a fee-bump; `hash` may be the outer or
    /// the inner hash, whichever the RPC was asked for.
    pub fee_bump: bool,
    /// Network name derived from the RPC's passphrase.
    pub network: String,
    /// The endpoint, or `fixture:<dir>`.
    pub rpc: String,
    /// Sequence number of the ledger the transaction was applied in.
    pub ledger: u32,
    /// Protocol version of that ledger, which picked the host that replayed it.
    pub protocol: u32,
    /// Close time of the transaction's ledger, seconds since the Unix epoch.
    pub close_time: u64,
    /// Always `failed`: sororewind only replays failures.
    pub status: &'static str,

    /// Whether the replay reproduced the network's outcome.
    pub fidelity: Fidelity,
    /// The network's recorded result and the replay's.
    pub result: ResultPair,
    /// `failure` and `calls`.
    #[serde(flatten)]
    pub story: Story,

    /// CPU and memory the replay used, against its limits and the network's.
    pub budget: BudgetUse,
    /// How faithfully the footprint's entries could be put back as they were.
    pub footprint: FootprintReport,
    /// Whether the host's PRNG seed matched the network's.
    pub prng_seed: PrngSeed,
    /// The diagnostic events of both runs.
    pub diagnostics: Diagnostics,
}

impl Report {
    /// Whether the replay matched the network, for the exit code.
    pub fn outcome(&self) -> Outcome {
        if self.fidelity.matches {
            Outcome::Matched
        } else {
            Outcome::Diverged
        }
    }
}

/// What a completed run concluded. Maps onto the process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The local replay reproduced the network's recorded result.
    Matched,
    /// The replay ran but disagreed with the network.
    Diverged,
}

/// The network's result next to the local one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResultPair {
    /// What the network recorded.
    pub onchain: ResultSummary,
    /// What the local replay produced.
    pub local: ResultSummary,
}

/// An `InvokeHostFunctionResult`, structured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultSummary {
    /// `success`, `trapped`, `resource_limit_exceeded`, `entry_archived`,
    /// `insufficient_refundable_fee` or `malformed`.
    pub code: &'static str,
    /// Hash of the return value and events; only for `success`.
    pub result_hash: Option<String>,
}

impl ResultSummary {
    /// The result in the words a person would use.
    pub fn label(&self) -> String {
        match &self.result_hash {
            Some(hash) => format!("success (result hash {hash})"),
            None => self.code.replace('_', " "),
        }
    }
}

impl From<&InvokeHostFunctionResult> for ResultSummary {
    fn from(result: &InvokeHostFunctionResult) -> Self {
        ResultSummary {
            code: display::result_code(result),
            result_hash: match result {
                InvokeHostFunctionResult::Success(hash) => Some(hex::encode(hash.0)),
                _ => None,
            },
        }
    }
}

/// How far the ledger has moved since the transaction ran.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FootprintReport {
    /// Number of keys in the transaction's footprint.
    pub entries: usize,
    /// Footprint slots the host was handed no entry for.
    pub missing: usize,
    /// Entries written at or after the transaction's own ledger — the value
    /// replayed may not be the value the host saw.
    pub drifted: Vec<String>,
    /// Entries whose TTL provably changed after the transaction ran.
    pub ttl_drifted: Vec<TtlDriftEntry>,
    /// Entries that were archived when the transaction ran and not restored
    /// by it.
    pub archived: Vec<String>,
    /// The archived entries whose TTL back then is lost: that they were
    /// archived is taken from the network's `ENTRY_ARCHIVED` result, so a
    /// match on that result is assumed rather than reproduced.
    pub archived_inferred: Vec<String>,
    /// Entries that changed since the transaction and were rebuilt, from
    /// ledger metadata, as the transaction saw them.
    pub rebuilt: Vec<String>,
    /// Why the entries still listed in `drifted` could not be rebuilt.
    pub rebuild_note: Option<String>,
    /// Entries the RPC returned without a `lastModifiedLedgerSeq`: whether
    /// they changed since the transaction is unknown.
    pub drift_unknown: Vec<String>,
    /// Classic entries the RPC returned without their extension; they were
    /// replayed with an empty one and may be smaller than they were.
    pub ext_missing: Vec<String>,
    /// Network settings modified at or after the transaction's ledger.
    pub config_drifted: Vec<String>,
    /// Network settings the RPC returned without a `lastModifiedLedgerSeq`.
    pub config_drift_unknown: Vec<String>,
}

/// A footprint entry whose TTL changed after the transaction ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TtlDriftEntry {
    /// The entry's ledger key, rendered for reading.
    pub entry: String,
    /// How its TTL changed.
    pub reason: TtlDriftReason,
}

/// How an entry's TTL changed since the transaction ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TtlDriftReason {
    /// The TTL was extended after the transaction ran.
    ExtendedSince,
    /// The entry was live for the transaction and has been archived since.
    ArchivedSince,
    /// The transaction listed the entry for restoration; something else has
    /// restored it since.
    RestoredSince,
}

impl TtlDriftReason {
    /// The reason as a short phrase for the human report.
    pub fn describe(self) -> &'static str {
        match self {
            TtlDriftReason::ExtendedSince => "TTL extended since",
            TtlDriftReason::ArchivedSince => "archived since",
            TtlDriftReason::RestoredSince => "restored since",
        }
    }
}

impl From<TtlDrift> for TtlDriftReason {
    fn from(drift: TtlDrift) -> Self {
        match drift {
            TtlDrift::ExtendedSince => TtlDriftReason::ExtendedSince,
            TtlDrift::ArchivedSince => TtlDriftReason::ArchivedSince,
            TtlDrift::RestoredSince => TtlDriftReason::RestoredSince,
        }
    }
}

/// Diagnostic events, one rendered line each, for side-by-side comparison,
/// and the events themselves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostics {
    /// The network's diagnostic events, one rendered line each; empty when
    /// the RPC kept none.
    pub onchain: Vec<String>,
    /// The replay's diagnostic events, one rendered line each.
    pub local: Vec<String>,
    /// Each on-chain `DiagnosticEvent`, base64 XDR, in the order of `onchain`.
    pub onchain_xdr: Vec<String>,
    /// Each local `DiagnosticEvent`, base64 XDR, in the order of `local`.
    pub local_xdr: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::Hash;

    #[test]
    fn summarises_results() {
        let trapped = ResultSummary::from(&InvokeHostFunctionResult::Trapped);
        assert_eq!(
            (trapped.code, trapped.result_hash.as_deref()),
            ("trapped", None)
        );
        assert_eq!(trapped.label(), "trapped");

        let limit = ResultSummary::from(&InvokeHostFunctionResult::ResourceLimitExceeded);
        assert_eq!(limit.label(), "resource limit exceeded");

        let success = ResultSummary::from(&InvokeHostFunctionResult::Success(Hash([1; 32])));
        assert_eq!(success.code, "success");
        assert_eq!(
            success.label(),
            format!("success (result hash {})", "01".repeat(32))
        );
    }
}
