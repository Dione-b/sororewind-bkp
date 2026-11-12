//! Snapshot tests over the recorded fixtures.
//!
//! Every directory under `fixtures/` is a complete offline RPC recording of
//! one failed transaction, on testnet unless its name starts with `mainnet-`
//! (see `src/transport.rs` for the layout).
//! Each fixture gets its own test, replayed once, in-process, through the
//! same pipeline the binary runs; the test pins the narrative, the exit code
//! and the JSON document together. Nothing here touches the network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use sororewind::Outcome;
use sororewind::render::{self, Detail};

async fn check(name: &str, expected: Outcome) {
    let result = common::replay(name).await;
    let report = result
        .as_ref()
        .unwrap_or_else(|err| panic!("fixture {name} could not be replayed: {err}"));

    // The narrative, exactly as a user sees it.
    let human = render::human(report, false, Detail::Summary);
    insta::assert_snapshot!(name, common::transcript(&result, &human));

    // The tool's core trust criterion: a fixture recorded as a match must
    // keep matching, and a fixture kept as a divergence must keep diverging.
    assert_eq!(
        report.outcome(),
        expected,
        "fixture {name}: wrong outcome; re-record it (see the README, \"Fixtures\")"
    );

    // The JSON document agrees with the narrative.
    let json: serde_json::Value = serde_json::from_str(&render::json(report).unwrap()).unwrap();
    assert_eq!(json["schemaVersion"], 1, "{name}");
    assert_eq!(
        json["hash"],
        common::fixture_hash(name).await.as_str(),
        "{name}"
    );
    assert_eq!(
        json["fidelity"]["match"],
        expected == Outcome::Matched,
        "{name}"
    );
    let network = if name.starts_with("mainnet-") {
        "mainnet"
    } else {
        "testnet"
    };
    assert_eq!(json["network"], network, "{name}");
    assert_eq!(json["status"], "failed", "{name}");
    assert_eq!(
        json["result"]["onchain"]["code"] == json["result"]["local"]["code"],
        json["fidelity"]["divergence"] != "result",
        "{name}"
    );
    assert_eq!(
        json["calls"].as_array().unwrap().len(),
        report.story.calls.len(),
        "{name}"
    );

    // CPU instructions are not compared: stellar-core's `core_metrics` count
    // work outside the host invocation (module cache, VM setup), so a
    // faithful replay can still report a different figure.
}

/// One test per fixture, so one stale snapshot does not hide the others.
macro_rules! fixtures {
    ($($test:ident: $name:literal => $outcome:ident,)*) => {
        $(
            #[tokio::test]
            async fn $test() {
                check($name, Outcome::$outcome).await;
            }
        )*

        const FIXTURES: &[&str] = &[$($name),*];
    };
}

fixtures! {
    auth_check_auth: "auth-check-auth" => Matched,
    auth_error: "auth-error" => Matched,
    budget_error: "budget-error" => Matched,
    contract_error: "contract-error" => Matched,
    storage_error: "storage-error" => Matched,
    // Failed stellar-core's refundable-fee check, after the host succeeded.
    refundable_fee: "refundable-fee" => Matched,
    // An oracle's price entry, rewritten every few ledgers: only matches
    // because the entry is rebuilt from ledger metadata.
    rebuilt_entry: "rebuilt-entry" => Matched,
    // Five contracts, calls three levels deep, a log with arguments, and a
    // contract error named from the failing contract's own spec.
    nested_contract_error: "nested-contract-error" => Matched,
    // Protocol 29, replayed by the older embedded host.
    mainnet_auth_error: "mainnet-auth-error" => Matched,
}

/// A fixture directory without a test is a recording nobody checks.
#[test]
fn every_fixture_has_a_test() {
    let mut on_disk: Vec<String> = std::fs::read_dir(common::fixtures_root())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = FIXTURES.iter().map(|name| name.to_string()).collect();
    listed.sort();
    assert_eq!(
        on_disk, listed,
        "fixtures/ and the `fixtures!` list disagree"
    );
}

/// Fixtures are committed; keep them small. The cap was raised from 1 MB on
/// purpose when fixtures started to keep the ledger entry changes their
/// replay rebuilds entries from: most of it is `mainnet-auth-error`, whose
/// footprint touches a busy pool. A new fixture that needs more room than
/// this should rather replace one.
#[test]
fn fixtures_stay_small() {
    let mut total = 0;
    for name in FIXTURES {
        total += std::fs::read_dir(common::fixture_dir(name))
            .unwrap()
            .filter_map(|entry| entry.ok()?.metadata().ok())
            .map(|meta| meta.len())
            .sum::<u64>();
    }
    assert!(total < 2_000_000, "fixtures/ is {total} bytes");
}

/// sororewind reads one ledger header, and a few ledgers' entry changes when
/// the footprint drifted. A recording holding `metadataXdr` itself, or long
/// runs of ledgers, means the recorder stopped slimming or the `getLedgers`
/// request lost its limit.
#[test]
fn fixtures_record_no_ledger_metadata() {
    for name in FIXTURES {
        for entry in std::fs::read_dir(common::fixture_dir(name)).unwrap() {
            let path = entry.unwrap().path();
            let file = path.file_name().unwrap().to_string_lossy();
            if !file.starts_with("getLedgers") {
                continue;
            }
            let body: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let ledgers = body["result"]["ledgers"].as_array().unwrap();
            assert!((1..=4).contains(&ledgers.len()), "{name}/{file}");
            for ledger in ledgers {
                assert!(ledger.get("metadataXdr").is_none(), "{name}/{file}");
                assert!(ledger.get("metadataJson").is_none(), "{name}/{file}");
            }
        }
    }
}

/// The other output modes, pinned for one fixture: `-v`, `-vv` and `--json`.
#[tokio::test]
async fn output_modes() {
    let name = "contract-error";
    let report = common::replay(name).await.unwrap();
    insta::assert_snapshot!(
        "contract-error-verbose",
        render::human(&report, false, Detail::Budget)
    );
    insta::assert_snapshot!(
        "contract-error-diagnostics",
        render::human(&report, false, Detail::Diagnostics)
    );
    // `rpc` names the fixture directory, which depends on the checkout.
    let mut json: serde_json::Value =
        serde_json::from_str(&render::json(&report).unwrap()).unwrap();
    json["rpc"] = "<fixture>".into();
    insta::assert_snapshot!(
        "contract-error-json",
        serde_json::to_string_pretty(&json).unwrap()
    );
}
