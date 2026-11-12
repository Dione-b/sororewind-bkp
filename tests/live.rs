//! Live end-to-end test against the public testnet RPC.
//!
//! Ignored by default (`cargo test` stays offline and deterministic); CI runs
//! it nightly with `cargo test --release --test live -- --ignored --nocapture`
//! (`just live`). It runs optimised because it times each replay, and an
//! unoptimised host interprets WASM several times slower.
//!
//! No hash is committed here: the RPC forgets transactions after about a
//! week, so the test asks the network for its most recent failures instead.
//! Some of those legitimately diverge (a contract that rewrites the same entry
//! every few ledgers cannot be replayed against current state, and one busy
//! contract can fill the whole recent history), so the test samples a few
//! failures of each kind and demands that every replay completes in time and
//! that at least one matches.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::time::{Duration, Instant};

use sororewind::discover::{Candidate, Search, recent_failures};
use sororewind::rpc::RpcClient;
use sororewind::transport::HttpTransport;
use sororewind::{Network, Outcome, Request};

/// How many recent failures of each category to replay.
const PER_CATEGORY: usize = 3;

/// The tool must answer in a few seconds; generous headroom for CI and for a
/// replay that fetches ledger metadata, but it still catches a pathological
/// RPC round-trip. Meant for a release build.
const TIME_BUDGET: Duration = Duration::from_secs(15);

#[tokio::test]
#[ignore = "requires network access to the testnet RPC"]
async fn recent_failures_replay_and_match() {
    let network = Network::Testnet;
    let rpc = RpcClient::new(HttpTransport::new(network.default_rpc_url()).unwrap());
    let search = Search {
        limit: 300,
        max_ledgers: 8_000,
    };
    let found = recent_failures(&rpc, search)
        .await
        .expect("could not list recent failures on testnet");
    let mut candidates = Vec::new();
    for candidate in found {
        let seen = candidates
            .iter()
            .filter(|c: &&Candidate| c.category == candidate.category)
            .count();
        if seen < PER_CATEGORY {
            candidates.push(candidate);
        }
    }
    assert!(
        !candidates.is_empty(),
        "testnet had no failed Soroban transaction in the last {} ledgers",
        search.max_ledgers
    );

    let mut matched = 0;
    let mut problems = Vec::new();
    for candidate in &candidates {
        let request = Request {
            hash: candidate.hash.clone(),
            network: Some(network),
            wasm_debug: None,
        };
        let started = Instant::now();
        let result = sororewind::run(&rpc, &request).await;
        let elapsed = started.elapsed();
        let verdict = match &result {
            Ok(report) => format!("{:?}", report.outcome()),
            Err(err) => format!("error: {err}"),
        };
        println!(
            "{} {:<14} {verdict} in {elapsed:.1?}",
            candidate.hash, candidate.category
        );

        match result {
            Ok(report) if report.outcome() == Outcome::Matched => matched += 1,
            Ok(_) => {}
            Err(err) => problems.push(format!("{}: {err}", candidate.hash)),
        }
        if elapsed > TIME_BUDGET {
            problems.push(format!("{}: took {elapsed:.1?}", candidate.hash));
        }
    }

    assert!(
        problems.is_empty(),
        "replays failed:\n{}",
        problems.join("\n")
    );
    assert!(
        matched > 0,
        "none of {} recent failures replayed as a match",
        candidates.len()
    );
}

/// The nightly signal for a protocol upgrade: this build replays the protocols
/// of its embedded hosts, so the day a network moves past the newest one,
/// sororewind stops working there until a host is added (see CONTRIBUTING.md,
/// "Protocol upgrades").
///
/// A network older than every embedded host is only reported: it cannot be
/// fixed by an upgrade here.
#[tokio::test]
#[ignore = "requires network access to the testnet and mainnet RPCs"]
async fn no_network_is_ahead_of_the_embedded_hosts() {
    let newest = sororewind::host::newest().protocol;
    let mut ahead = Vec::new();
    for network in [Network::Testnet, Network::Mainnet] {
        let rpc = RpcClient::new(HttpTransport::new(network.default_rpc_url()).unwrap());
        let passphrase = rpc.network().await.unwrap().passphrase;
        let latest = rpc.health().await.unwrap().latest_ledger.unwrap();
        let ledger = sororewind::state::read_ledger(&rpc, latest, &passphrase)
            .await
            .unwrap();
        let protocol = ledger.protocol;
        if let Some(host) = sororewind::host::for_protocol(protocol) {
            println!(
                "{network:?}: protocol {protocol}, replayed by soroban-env-host {}",
                host.version
            );
        } else if protocol > newest {
            ahead.push(format!("{network:?} runs protocol {protocol}"));
        } else {
            println!(
                "{network:?}: protocol {protocol}, older than every embedded host ({})",
                sororewind::host::describe()
            );
        }
    }
    assert!(
        ahead.is_empty(),
        "this build embeds protocols {}, but {}; add a host for the new protocol \
         (CONTRIBUTING.md, \"Protocol upgrades\")",
        sororewind::host::describe(),
        ahead.join(" and ")
    );
}
