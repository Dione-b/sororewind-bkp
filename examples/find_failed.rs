//! List recent failed Soroban transactions on a live network.
//!
//! ```text
//! cargo run --example find_failed -- [--network testnet] [--limit 20] [--ledgers 2000]
//! ```
//!
//! Prints one `category<TAB>hash<TAB>ledger<TAB>result` line per transaction,
//! most recent first. Use it to pick fresh hashes for `record_fixture`: the
//! hashes in a fixture expire from the RPC after about a week, so new
//! fixtures always start from a transaction found this way.

// A command-line tool: printing is what it is for.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use clap::Parser;
use sororewind::Network;
use sororewind::discover::{Search, recent_failures};
use sororewind::network::NetworkId;
use sororewind::report::ResultSummary;
use sororewind::rpc::RpcClient;
use sororewind::transport::HttpTransport;

#[derive(Parser)]
struct Args {
    #[arg(long, short = 'n', value_enum, default_value = "testnet")]
    network: Network,
    /// Use this RPC endpoint instead of the default for --network
    #[arg(long)]
    rpc_url: Option<String>,
    /// Stop after this many transactions
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Look at most this many ledgers back (~5 seconds each)
    #[arg(long, default_value_t = 2_000)]
    ledgers: u32,
}

/// Scan `rpc`, once it is known to serve `network`: `--network mainnet` with
/// a testnet `--rpc-url` must not quietly list testnet transactions.
async fn scan(
    rpc: &RpcClient,
    network: Network,
    search: Search,
) -> sororewind::Result<Vec<sororewind::discover::Candidate>> {
    let actual = NetworkId::from_passphrase(&rpc.network().await?.passphrase);
    if actual != NetworkId::Known(network) {
        return Err(sororewind::RewindError::NetworkMismatch {
            expected: network,
            actual,
            origin: rpc.origin(),
        });
    }
    recent_failures(rpc, search).await
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let url = args
        .rpc_url
        .unwrap_or_else(|| args.network.default_rpc_url().to_string());
    let search = Search {
        limit: args.limit,
        max_ledgers: args.ledgers,
    };

    let found = match HttpTransport::new(url) {
        Ok(http) => scan(&RpcClient::new(http), args.network, search).await,
        Err(err) => Err(err),
    };
    match found {
        Ok(found) => {
            for candidate in found {
                println!(
                    "{}\t{}\t{}\t{}",
                    candidate.category,
                    candidate.hash,
                    candidate.ledger,
                    ResultSummary::from(&candidate.result).label()
                );
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("find_failed: {err}");
            ExitCode::FAILURE
        }
    }
}
