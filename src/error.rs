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

/// Every failure a run can end in. `Display` is the message for the terminal;
/// [`RewindError::kind`] is the stable name for the `--json` error document.
#[derive(Debug, Error)]
pub enum RewindError {
    /// The hash given on the command line is not 64 hex characters.
    #[error("invalid transaction hash `{hash}`: expected 64 hexadecimal characters (32 bytes)")]
    InvalidHash {
        /// The hash as the user typed it.
        hash: String,
    },

    /// Neither a hash nor a `--fixture` to read one from was given.
    #[error("no transaction hash was given, and there is no --fixture to take one from")]
    MissingHash,

    /// `SOROREWIND_RPC_URL` is not an http(s) URL with a host (`--rpc-url`
    /// itself is checked by clap).
    #[error("invalid RPC URL: {reason}")]
    InvalidRpcUrl {
        /// Why the URL was rejected.
        reason: String,
    },

    /// The report could not be serialized or written to stdout; holds the cause.
    #[error("could not write the report: {0}")]
    Output(String),

    /// `getTransaction` answered `NOT_FOUND` for the hash.
    #[error(
        "transaction {hash} not found on {network}.\n\
         \n\
         {retention}\n\
         The transaction is either older than that, on a different network, or the hash is wrong.\n\
         \n\
         Tip: pass --network, or --rpc-url to point at another RPC."
    )]
    NotFound {
        /// The transaction that was looked up.
        hash: TxHash,
        /// The network the RPC serves.
        network: NetworkId,
        /// What the RPC said about how far back it keeps transactions.
        retention: Retention,
    },

    /// No answer came back from the RPC: DNS, connection, TLS or a timeout,
    /// after any retries.
    #[error("cannot reach the RPC at {url}: {detail}")]
    RpcUnreachable {
        /// The endpoint, redacted: see `transport::redact`.
        url: String,
        /// The whole cause chain, innermost last.
        detail: String,
        /// The underlying HTTP client error.
        #[source]
        source: reqwest::Error,
    },

    /// The `reqwest` client could not be built (for example, no TLS backend).
    #[error("could not set up the HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),

    /// The RPC answered with a JSON-RPC error, a non-2xx HTTP status, or a
    /// response that makes no sense.
    #[error("the RPC ({origin}) returned an error for {method}: {detail}{}", code_suffix(*.code))]
    Rpc {
        /// Where the answer came from: the redacted RPC URL, or `fixture:<dir>`.
        origin: String,
        /// The JSON-RPC method that failed, such as `getTransaction`.
        method: String,
        /// The JSON-RPC error code, when the failure was a JSON-RPC error.
        code: Option<i64>,
        /// The RPC's error message, the HTTP status and body, or what was wrong
        /// with its answer.
        detail: String,
    },

    /// `--network` names a network other than the one the RPC's passphrase
    /// says it serves.
    #[error(
        "--network {expected} was requested, but {origin} serves {actual}.\n\
         \n\
         Drop --network to trust the RPC's own passphrase, or point --rpc-url at a {expected} endpoint."
    )]
    NetworkMismatch {
        /// The network `--network` asked for.
        expected: Network,
        /// The network the RPC reported via `getNetwork`.
        actual: NetworkId,
        /// The redacted RPC URL, or `fixture:<dir>`.
        origin: String,
    },

    /// The transaction has no `InvokeHostFunction` operation to replay.
    #[error(
        "transaction {hash} is not a Soroban transaction: {reason}\n\
         \n\
         sororewind only replays transactions that invoke a host function \
         (contract calls, token operations, contract deployment)."
    )]
    NotSoroban {
        /// The transaction that was looked up.
        hash: TxHash,
        /// What the transaction is instead.
        reason: String,
    },

    /// The transaction failed in stellar-core before any contract code ran.
    #[error(
        "transaction {hash} failed before it reached the Soroban host (transaction result `{code}`).\n\
         \n\
         stellar-core rejected it during validation or fee processing, so no contract code ran \
         and there is nothing to replay."
    )]
    NeverReachedHost {
        /// The failed transaction.
        hash: TxHash,
        /// The `TransactionResultCode` name, such as `TxBadSeq`.
        code: &'static str,
    },

    /// The transaction's ledger ran on a protocol none of the embedded hosts
    /// execute.
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
        /// The transaction that cannot be replayed.
        hash: TxHash,
        /// The protocol version of the ledger the transaction ran in.
        tx_protocol: u32,
        /// The protocols this build replays, with their host versions.
        supported: String,
        /// The network the transaction is on.
        network: NetworkId,
    },

    /// The transaction succeeded, so there is no failure to narrate.
    #[error(
        "transaction {hash} succeeded on-chain; there is nothing to explain.\n\
         sororewind narrates failures."
    )]
    NotAFailure {
        /// The successful transaction.
        hash: TxHash,
    },

    /// A ledger header needed for replay is outside the RPC's retention window.
    #[error(
        "the RPC has no ledger {sequence} in its retention window; \
         it can no longer tell us the state that transaction ran against"
    )]
    LedgerUnavailable {
        /// The ledger sequence number that could not be fetched.
        sequence: u32,
    },

    /// A network config setting the host needs was absent from
    /// `getLedgerEntries`.
    #[error(
        "the RPC did not return the network setting `{setting}`, so the replay cannot be \
         priced the way the network priced it"
    )]
    ConfigSettingMissing {
        /// The `ConfigSettingId` name, such as `ContractComputeV0`.
        setting: &'static str,
    },

    /// A network config setting came back with the wrong shape or an unusable
    /// value.
    #[error("the network setting `{setting}` returned by the RPC is not usable: {detail}")]
    ConfigSettingMalformed {
        /// The `ConfigSettingId` name.
        setting: &'static str,
        /// What was wrong with it.
        detail: String,
    },

    /// A contract data or code entry came back without its TTL.
    #[error(
        "the RPC returned {entry} without a liveUntilLedgerSeq, so whether it was live when \
         the transaction ran cannot be established"
    )]
    MissingTtl {
        /// The ledger key of the entry, as text.
        entry: String,
    },

    /// A file in the `--fixture` directory could not be read.
    #[error("could not read {path}: {source}")]
    FixtureIo {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The hash given on the command line differs from the one the fixture
    /// records.
    #[error(
        "the fixture at {dir} was recorded for transaction {recorded}, not {requested}.\n\
         \n\
         Drop the hash to replay the transaction the fixture holds."
    )]
    FixtureHashMismatch {
        /// The `--fixture` directory.
        dir: PathBuf,
        /// The transaction the fixture was recorded for.
        recorded: TxHash,
        /// The transaction the command line asked for.
        requested: TxHash,
    },

    /// A fixture file exists but cannot be parsed or is inconsistent.
    #[error("{path} is not a usable fixture: {reason}")]
    FixtureInvalid {
        /// The offending file or directory.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },

    /// Replay made a call the fixture has no recorded response for.
    #[error(
        "the fixture at {dir} has no recorded answer for {what}; it was recorded for a \
         different transaction"
    )]
    FixtureMissing {
        /// The `--fixture` directory.
        dir: PathBuf,
        /// The call or ledger key that was not recorded.
        what: String,
    },

    /// Writing a `--record` fixture file failed.
    #[error("could not write the recording to {path}: {source}")]
    RecordIo {
        /// The file that could not be written.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The `--wasm-debug` file could not be read.
    #[error("could not read --wasm-debug {path}: {source}")]
    WasmFile {
        /// The path given to `--wasm-debug`.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// XDR from the RPC did not decode.
    #[error("failed to decode {what} returned by the RPC: {source}")]
    XdrDecode {
        /// Which field or value failed, such as `envelopeXdr`.
        what: &'static str,
        /// The XDR decoding error.
        #[source]
        source: stellar_xdr::Error,
    },

    /// A base64 XDR string from the RPC was not valid base64.
    #[error("invalid base64 in {what} returned by the RPC: {source}")]
    Base64 {
        /// Which field failed, such as `resultXdr`.
        what: &'static str,
        /// The base64 decoding error.
        #[source]
        source: base64::DecodeError,
    },

    /// A value could not be encoded to XDR to hand to the Soroban host.
    #[error("could not encode {what} for the host: {source}")]
    XdrEncode {
        /// Which value failed.
        what: &'static str,
        /// The XDR encoding error.
        #[source]
        source: stellar_xdr::Error,
    },

    /// An RPC response lacks a field sororewind needs.
    #[error("the {method} response is missing `{field}`")]
    MissingField {
        /// The JSON-RPC method whose response was incomplete.
        method: &'static str,
        /// The missing field's JSON name.
        field: &'static str,
    },

    /// The contract's WASM has no usable `contractspecv0` error enum.
    #[error("the contract declares no error names: {reason}")]
    SpecMissing {
        /// What was missing or malformed in the spec.
        reason: String,
    },

    /// The failing contract's WASM could not be found in the replayed state.
    #[error("the contract's code could not be located: {reason}")]
    ContractCode {
        /// Which step of the lookup failed.
        reason: &'static str,
    },

    /// The Soroban host could not be set up or returned something unusable;
    /// holds the reason.
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
    /// `getHealth` answered with its window.
    Reported {
        /// The retention window, in ledgers.
        window: u32,
        /// The oldest ledger the RPC still holds, when it said.
        oldest: Option<u32>,
    },
    /// `getHealth` failed or did not report a window.
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

/// The crate's `Result`, failing with a [`RewindError`].
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
        assert!(
            error(Some(-32600))
                .to_string()
                .ends_with("(JSON-RPC code -32600)")
        );
        assert!(error(None).to_string().ends_with("for this rpc instance"));
        assert!(error(None).is_ledger_out_of_range());
        assert_eq!(error(None).kind(), "rpc_error");
    }
}
