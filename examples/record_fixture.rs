//! Record a failed transaction as an offline RPC fixture.
//!
//! ```text
//! cargo run --example record_fixture -- <name> <hash>   # record one
//! cargo run --example record_fixture                    # re-record all
//! ```
//!
//! The recording is staged next to the target (in a hidden directory) and
//! only replaces `fixtures/<name>` once the replay has run, so a failed or
//! interrupted recording never destroys a usable fixture. By default only a replay that *matches* the
//! network is kept; pass `--allow-diverged` to keep a divergence on purpose
//! (that is how the `diverged-*` fixtures were made).
//!
//! Without arguments every existing fixture is re-recorded from the hash in
//! its `fixture.json`. That only works while the RPC still has the
//! transaction (about a week); after that, find a fresh one with
//! `find_failed`. Fixtures that replay as a divergence are skipped: they
//! captured a ledger that had already moved and cannot be reproduced.

// A command-line tool: printing is what it is for.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use sororewind::network::NetworkId;
use sororewind::rpc::RpcClient;
use sororewind::transport::{FixtureManifest, FixtureTransport, HttpTransport, Recording};
use sororewind::{Network, Outcome, Request, Result, TxHash};

#[derive(Parser)]
struct Args {
    /// Fixture name: a directory under fixtures/
    #[arg(requires = "hash")]
    name: Option<String>,
    /// Transaction to record
    hash: Option<TxHash>,
    #[arg(long, short = 'n', value_enum, default_value = "testnet")]
    network: Network,
    /// Use this RPC endpoint instead of the default for --network
    #[arg(long)]
    rpc_url: Option<String>,
    /// Keep the recording even when the replay diverges from the network
    #[arg(long)]
    allow_diverged: bool,
    /// Directory holding the fixtures
    #[arg(long, default_value = "fixtures")]
    dir: PathBuf,
}

/// What became of one recording attempt.
enum Recorded {
    Kept(Outcome),
    /// The replay diverged and `--allow-diverged` was not given.
    Rejected,
    Skipped(&'static str),
}

/// `network` is the network the RPC is expected to serve; `None` trusts the
/// RPC's own passphrase, which is the only option for a custom network.
async fn replay(rpc: &RpcClient, hash: &TxHash, network: Option<Network>) -> Result<Outcome> {
    let request = Request {
        hash: hash.clone(),
        network,
        wasm_debug: None,
    };
    Ok(sororewind::run(rpc, &request).await?.outcome())
}

/// Record `hash` as `name`. `rpc_network` picks the default endpoint;
/// `expected` is the network to hold the RPC to, if any.
async fn record(
    args: &Args,
    name: &str,
    hash: &TxHash,
    rpc_network: Network,
    expected: Option<Network>,
) -> Result<Recorded> {
    let url = args
        .rpc_url
        .clone()
        .unwrap_or_else(|| rpc_network.default_rpc_url().to_string());
    // The recording is staged next to the target and only replaces it when
    // committed; see `Recording`.
    let recording = Recording::create(HttpTransport::new(url)?, args.dir.join(name), hash).await?;
    let handle = recording.handle();
    let outcome = replay(&RpcClient::new(recording), hash, expected).await;

    let keep = match &outcome {
        Ok(Outcome::Matched) => true,
        Ok(Outcome::Diverged) => args.allow_diverged,
        Err(_) => false,
    };
    if !keep {
        handle.discard().await;
        return outcome.map(|_| Recorded::Rejected);
    }
    handle.commit().await?;
    outcome.map(Recorded::Kept)
}

/// Every fixture under `dir`, with the hash it was recorded for. Anything
/// else in there is reported and left alone.
async fn existing(dir: &Path) -> Result<Vec<(String, TxHash)>> {
    let mut found = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|source| sororewind::RewindError::FixtureIo {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.path().is_dir() || name.starts_with('.') {
            continue;
        }
        match FixtureManifest::load(&entry.path())
            .await
            .and_then(|m| m.tx_hash())
        {
            Ok(hash) => found.push((name, hash)),
            Err(err) => eprintln!("{name}: skipped, {err}"),
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

async fn rerecord(args: &Args, name: &str, hash: &TxHash) -> Result<Recorded> {
    // A fixture that already diverges offline captured drift on purpose.
    let offline = RpcClient::new(FixtureTransport::open(args.dir.join(name)).await?);
    // Re-record from the network the fixture was recorded on. A custom
    // network has no default endpoint and no name to check: it needs
    // `--rpc-url`, and the RPC's passphrase is taken at its word.
    let (rpc_network, expected) =
        match NetworkId::from_passphrase(&offline.network().await?.passphrase) {
            NetworkId::Known(network) => (network, Some(network)),
            NetworkId::Custom(_) if args.rpc_url.is_none() => {
                return Ok(Recorded::Skipped(
                    "it was recorded on a custom network; pass --rpc-url to re-record it",
                ));
            }
            NetworkId::Custom(_) => (args.network, None),
        };
    if matches!(
        replay(&offline, hash, expected).await,
        Ok(Outcome::Diverged)
    ) {
        return Ok(Recorded::Skipped(
            "it captures a divergence, which cannot be re-recorded",
        ));
    }
    record(args, name, hash, rpc_network, expected).await
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let single = args.name.clone().zip(args.hash.clone());
    if let Some((name, _)) = &single
        && (name.is_empty() || name.starts_with('.') || name.contains(['/', '\\']))
    {
        eprintln!("record_fixture: invalid fixture name `{name}`");
        return ExitCode::FAILURE;
    }
    let jobs = match single.clone() {
        Some(job) => vec![job],
        None => match existing(&args.dir).await {
            Ok(jobs) => jobs,
            Err(err) => {
                eprintln!("record_fixture: {err}");
                return ExitCode::FAILURE;
            }
        },
    };

    let mut failed = false;
    for (name, hash) in &jobs {
        let result = if single.is_some() {
            record(&args, name, hash, args.network, Some(args.network)).await
        } else {
            rerecord(&args, name, hash).await
        };
        match result {
            Ok(Recorded::Kept(Outcome::Matched)) => println!("{name}: recorded ({hash})"),
            Ok(Recorded::Kept(Outcome::Diverged)) => {
                println!("{name}: recorded a divergence ({hash})");
            }
            Ok(Recorded::Skipped(why)) => println!("{name}: skipped, {why}"),
            Ok(Recorded::Rejected) => {
                failed = true;
                eprintln!(
                    "{name}: the replay diverged from the network; the previous fixture was kept.\n  \
                     Use a more recent transaction, or pass --allow-diverged to keep it."
                );
            }
            Err(err) => {
                failed = true;
                eprintln!("{name}: {err}\n  the previous fixture was kept");
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
