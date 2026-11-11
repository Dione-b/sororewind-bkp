//! Live end-to-end test against the public testnet RPC.
//!
//! Ignored by default (`cargo test` stays offline and deterministic); CI runs
//! it nightly with `cargo test --test live -- --ignored --nocapture`.
//!
//! No hash is committed here: the RPC forgets transactions after about a
//! week, so the test asks the network for its most recent failures instead.
//! Some of those legitimately diverge (a contract that rewrites the same entry
//! every few ledgers cannot be replayed against current state, and one busy
//! contract can fill the whole recent history), so the test samples a few
//! failures of each kind and demands that every replay completes in time and
//! that at least one matches.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use sororewind::discover::{recent_failures, Candidate, Search};
use sororewind::rpc::RpcClient;
use sororewind::transport::HttpTransport;
use sororewind::{Network, Outcome, Request};

/// How many recent failures of each category to replay.
const PER_CATEGORY: usize = 3;

/// The tool must answer in a few seconds; generous headroom for CI, but it
/// still catches a pathological RPC round-trip.
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
