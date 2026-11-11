//! Every way a run can fail, worded for the person reading the terminal.
//!
//! Each variant describes one situation and carries a message that is safe to
//! print verbatim: no backtrace, no Rust type names. Where an underlying error
//! exists it is kept as `#[source]` so the cause chain survives.

use std::fmt;
use std::path::PathBuf;

use thiserror::Error;

use crate::hash::TxHash;
use crate::network::{Network, NetworkId};

/// Stellar RPC's stock `HISTORY_RETENTION_WINDOW`, in ledgers (about 7 days).
pub const DEFAULT_RETENTION_WINDOW: u32 = 120_960;

#[derive(Debug, Error)]
pub enum RewindError {
    #[error("invalid transaction hash `{hash}`: expected 64 hexadecimal characters (32 bytes)")]
    InvalidHash { hash: String },

    #[error("no transaction hash was given, and there is no --fixture to take one from")]
    MissingHash,

    #[error("invalid RPC URL: {reason}")]
    InvalidRpcUrl { reason: String },

    #[error("could not write the report: {0}")]
    Output(String),

    #[error(
        "transaction {hash} not found on {network}.\n\
         \n\
         {retention}\n\
         The transaction is either older than that, on a different network, or the hash is wrong.\n\
         \n\
         Tip: pass --network, or --rpc-url to point at another RPC."
    )]
    NotFound {
        hash: TxHash,
        network: NetworkId,
        retention: Retention,
    },

    #[error("cannot reach the RPC at {url}: {detail}")]
    RpcUnreachable {
        /// The endpoint, redacted: see `transport::redact`.
        url: String,
        /// The whole cause chain, innermost last.
        detail: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("could not set up the HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),

    #[error("the RPC ({origin}) returned an error for {method}: {detail}{}", code_suffix(*.code))]
    Rpc {
        origin: String,
        method: String,
        /// The JSON-RPC error code, when the failure was a JSON-RPC error.
        code: Option<i64>,
        detail: String,
    },

    #[error(
        "--network {expected} was requested, but {origin} serves {actual}.\n\
         \n\
         Drop --network to trust the RPC's own passphrase, or point --rpc-url at a {expected} endpoint."
    )]
    NetworkMismatch {
        expected: Network,
        actual: NetworkId,
        origin: String,
    },

    #[error(
        "transaction {hash} is not a Soroban transaction: {reason}\n\
         \n\
         sororewind only replays transactions that invoke a host function \
         (contract calls, token operations, contract deployment)."
    )]
    NotSoroban { hash: TxHash, reason: String },

    #[error(
        "transaction {hash} failed before it reached the Soroban host (transaction result `{code}`).\n\
         \n\
         stellar-core rejected it during validation or fee processing, so no contract code ran \
         and there is nothing to replay."
    )]
    NeverReachedHost { hash: TxHash, code: &'static str },

    #[error(
        "cannot replay transaction {hash}: it ran on protocol {tx_protocol}, but this build of\n\
         sororewind embeds protocols {supported}.\n\
         \n\
         The Soroban host refuses to execute a ledger whose protocol version differs from its own,\n\
         so replay would be meaningless. Use a sororewind build that embeds soroban-env-host \
         {tx_protocol}.x\n\
         (the host's major version is the protocol version it executes).\n\
         \n\
         Network: {network}"
    )]
    ProtocolMismatch {
        hash: TxHash,
        tx_protocol: u32,
        /// The protocols this build replays, with their host versions.
        supported: String,
        network: NetworkId,
    },

    #[error(
        "transaction {hash} succeeded on-chain; there is nothing to explain.\n\
         sororewind narrates failures."
    )]
    NotAFailure { hash: TxHash },

    #[error(
        "the RPC has no ledger {sequence} in its retention window; \
         it can no longer tell us the state that transaction ran against"
    )]
    LedgerUnavailable { sequence: u32 },

    #[error(
        "the RPC did not return the network setting `{setting}`, so the replay cannot be \
         priced the way the network priced it"
    )]
    ConfigSettingMissing { setting: &'static str },

    #[error("the network setting `{setting}` returned by the RPC is not usable: {detail}")]
    ConfigSettingMalformed {
        setting: &'static str,
        detail: String,
    },

    #[error(
        "the RPC returned {entry} without a liveUntilLedgerSeq, so whether it was live when \
         the transaction ran cannot be established"
    )]
    MissingTtl { entry: String },

    #[error("could not read {path}: {source}")]
    FixtureIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "the fixture at {dir} was recorded for transaction {recorded}, not {requested}.\n\
         \n\
         Drop the hash to replay the transaction the fixture holds."
    )]
    FixtureHashMismatch {
        dir: PathBuf,
        recorded: TxHash,
        requested: TxHash,
    },

    #[error("{path} is not a usable fixture: {reason}")]
    FixtureInvalid { path: PathBuf, reason: String },

    #[error(
        "the fixture at {dir} has no recorded answer for {what}; it was recorded for a \
         different transaction"
    )]
    FixtureMissing { dir: PathBuf, what: String },

    #[error("could not write the recording to {path}: {source}")]
    RecordIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not read --wasm-debug {path}: {source}")]
    WasmFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to decode {what} returned by the RPC: {source}")]
    XdrDecode {
        what: &'static str,
        #[source]
        source: stellar_xdr::Error,
    },

    #[error("invalid base64 in {what} returned by the RPC: {source}")]
    Base64 {
        what: &'static str,
        #[source]
        source: base64::DecodeError,
    },

    #[error("could not encode {what} for the host: {source}")]
    XdrEncode {
        what: &'static str,
        #[source]
        source: stellar_xdr::Error,
    },

    #[error("the {method} response is missing `{field}`")]
    MissingField {
        method: &'static str,
        field: &'static str,
    },

    #[error("the contract declares no error names: {reason}")]
    SpecMissing { reason: String },

    #[error("the contract's code could not be located: {reason}")]
    ContractCode { reason: &'static str },

    #[error("replay could not be started: {0}")]
    Replay(String),
}

fn code_suffix(code: Option<i64>) -> String {
    code.map_or(String::new(), |code| format!(" (JSON-RPC code {code})"))
}

impl RewindError {
    /// stellar-rpc's answer to a `startLedger` outside what it keeps: "start
    /// ledger (N) must be between the oldest ledger: A and the latest ledger:
    /// B for this rpc instance", code -32600 (checked against testnet and
    /// mainnet, 2026-11-11).
    pub fn is_ledger_out_of_range(&self) -> bool {
        matches!(
            self,
            RewindError::Rpc { detail, .. } if detail.contains("must be between the oldest ledger")
        )
    }

    /// A stable machine name for the kind of failure, for the `--json` error
    /// document. Names are part of the public interface: add, never rename.
    pub fn kind(&self) -> &'static str {
        match self {
            RewindError::InvalidHash { .. } => "invalid_hash",
            RewindError::MissingHash => "missing_hash",
            RewindError::InvalidRpcUrl { .. } => "invalid_rpc_url",
            RewindError::Output(_) => "output",
            RewindError::FixtureHashMismatch { .. } => "fixture_hash_mismatch",
            RewindError::NotFound { .. } => "not_found",
            RewindError::RpcUnreachable { .. } => "rpc_unreachable",
            RewindError::HttpClient(_) => "http_client",
            RewindError::Rpc { .. } => "rpc_error",
            RewindError::NetworkMismatch { .. } => "network_mismatch",
            RewindError::NotSoroban { .. } => "not_soroban",
            RewindError::NeverReachedHost { .. } => "never_reached_host",
            RewindError::ProtocolMismatch { .. } => "protocol_mismatch",
            RewindError::NotAFailure { .. } => "not_a_failure",
            RewindError::LedgerUnavailable { .. } => "ledger_unavailable",
            RewindError::ConfigSettingMissing { .. } => "config_setting_missing",
            RewindError::ConfigSettingMalformed { .. } => "config_setting_malformed",
            RewindError::MissingTtl { .. } => "missing_ttl",
            RewindError::FixtureIo { .. } => "fixture_io",
            RewindError::FixtureInvalid { .. } => "fixture_invalid",
            RewindError::FixtureMissing { .. } => "fixture_missing",
            RewindError::RecordIo { .. } => "record_io",
            RewindError::WasmFile { .. } => "wasm_file",
            RewindError::XdrDecode { .. } | RewindError::Base64 { .. } => "malformed_response",
            RewindError::XdrEncode { .. } => "xdr_encode",
            RewindError::MissingField { .. } => "missing_field",
            RewindError::SpecMissing { .. } => "spec_missing",
            RewindError::ContractCode { .. } => "contract_code",
            RewindError::Replay(_) => "replay",
        }
    }

    pub(crate) fn fixture_path(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| RewindError::FixtureIo { path, source }
    }

    pub(crate) fn record_path(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| RewindError::RecordIo { path, source }
    }
}

/// What the RPC's health probe said about its retention window, if it
/// answered at all. Only used to word [`RewindError::NotFound`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    Reported { window: u32, oldest: Option<u32> },
    Unknown,
}

impl fmt::Display for Retention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Retention::Reported { window, oldest } => {
                write!(
                    f,
                    "The RPC only keeps transactions for its retention window ({window} ledgers, \
                     about {}).",
                    approximate_duration(*window)
                )?;
                match oldest {
                    Some(oldest) => write!(f, "\nOldest ledger available: {oldest}."),
                    None => Ok(()),
                }
            }
            Retention::Unknown => write!(
                f,
                "The RPC only keeps transactions for its retention window (about 7 days with \
                 the default of {DEFAULT_RETENTION_WINDOW} ledgers)."
            ),
        }
    }
}

/// How long `ledgers` ledgers take, to the nearest sensible unit: "7 days",
/// "1 day", "5 hours", "less than an hour".
fn approximate_duration(ledgers: u32) -> String {
    // Ledgers close roughly every five seconds.
    let seconds = u64::from(ledgers) * 5;
    let unit = |n: u64, name: &str| format!("{n} {name}{}", if n == 1 { "" } else { "s" });
    // Round to the nearest unit, not down: 23.9 hours is a day.
    match seconds {
        s if s >= 86_400 - 1_800 => unit((s + 43_200) / 86_400, "day"),
        s if s >= 1_800 => unit((s + 1_800) / 3_600, "hour"),
        _ => "less than an hour".to_string(),
    }
}

pub type Result<T> = std::result::Result<T, RewindError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_windows_read_in_a_sensible_unit() {
        assert_eq!(approximate_duration(120_960), "7 days");
        assert_eq!(approximate_duration(17_280), "1 day");
        // Just under a day rounds to a day, not to "0 days".
        assert_eq!(approximate_duration(17_000), "1 day");
        assert_eq!(approximate_duration(8_640), "12 hours");
        assert_eq!(approximate_duration(720), "1 hour");
        assert_eq!(approximate_duration(100), "less than an hour");

        let text = Retention::Reported {
            window: 2_880,
            oldest: Some(5),
        }
        .to_string();
        assert!(text.contains("(2880 ledgers, about 4 hours)"), "{text}");
        assert!(text.ends_with("Oldest ledger available: 5."), "{text}");
        let text = Retention::Reported {
            window: 2_880,
            oldest: None,
        }
        .to_string();
        assert!(!text.contains("Oldest"), "{text}");
    }

    #[test]
    fn rpc_errors_show_their_code() {
        let error = |code| RewindError::Rpc {
            origin: "https://rpc.example".to_string(),
            method: "getLedgers".to_string(),
            code,
            detail: "start ledger (5) must be between the oldest ledger: 9 and the latest \
                     ledger: 10 for this rpc instance"
                .to_string(),
        };
        assert!(error(Some(-32600))
            .to_string()
            .ends_with("(JSON-RPC code -32600)"));
        assert!(error(None).to_string().ends_with("for this rpc instance"));
        assert!(error(None).is_ledger_out_of_range());
        assert_eq!(error(None).kind(), "rpc_error");
    }
}
