use thiserror::Error;

/// All user-facing failures. Each variant carries a message that is safe to
/// print verbatim, with no backtrace and no Rust type names.
#[derive(Debug, Error)]
pub enum RewindError {
    #[error("invalid transaction hash `{hash}`: {reason}")]
    InvalidHash { hash: String, reason: String },

    #[error(
        "transaction {hash} not found on {network}.\n\
         \n\
         The RPC only keeps transactions for its retention window (~7 days, {window} ledgers).\n\
         Oldest ledger available: {oldest}. The transaction is either older than that,\n\
         on a different network, or the hash is wrong.\n\
         \n\
         Tip: pass --network testnet|mainnet, or --rpc-url to point at another RPC."
    )]
    NotFound {
        hash: String,
        network: String,
        window: u32,
        oldest: u32,
    },

    #[error("cannot reach the {network} RPC at {url}: {detail}")]
    RpcUnreachable {
        network: String,
        url: String,
        detail: String,
    },

    #[error("the {network} RPC returned an error for {method}: {detail}")]
    Rpc {
        network: String,
        method: String,
        detail: String,
    },

    #[error("transaction {hash} is not a Soroban transaction: {reason}\n\nsororewind only replays transactions that invoke a host function (contract calls, token operations, contract deployment).")]
    NotSoroban { hash: String, reason: String },

    #[error(
        "cannot replay transaction {hash}: it ran on protocol {tx_protocol}, but this build of\n\
         sororewind embeds protocol {host_protocol} (soroban-env-host {host_version}).\n\
         \n\
         The Soroban host refuses to execute a ledger whose protocol version differs from its own,\n\
         so replay would be meaningless. Rebuild sororewind against soroban-env-host {wanted}.\n\
         \n\
         Network: {network}"
    )]
    ProtocolMismatch {
        hash: String,
        tx_protocol: u32,
        host_protocol: u32,
        host_version: &'static str,
        wanted: u32,
        network: String,
    },

    #[error("transaction {hash} succeeded on-chain ({status}); there is nothing to explain.\nsororewind narrates failures.")]
    NotAFailure { hash: String, status: String },

    #[error("could not reconstruct the ledger state needed for replay: {0}")]
    State(String),

    #[error("replay could not be started: {0}")]
    Replay(String),

    #[error("failed to decode XDR returned by the RPC: {0}")]
    Decode(String),
}

pub type Result<T> = std::result::Result<T, RewindError>;
