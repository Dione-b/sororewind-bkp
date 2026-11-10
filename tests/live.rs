//! Live end-to-end test against the public testnet RPC.
//!
//! Ignored by default (`cargo test` stays offline and deterministic); run it
//! nightly with `cargo test --test live -- --ignored --nocapture`.
//!
//! It asserts the two properties that make the tool trustworthy on a real
//! network: a known-failed transaction replays, and the replay matches the
//! network's recorded result.

use std::process::Command;
use std::time::Instant;

/// A failed testnet `invoke_host_function` transaction known to replay-match.
/// If it ages out of the RPC's ~7-day retention window this test fails — that
/// is the signal to pick a fresh hash (see scripts/find-fixtures.sh).
const KNOWN_FAILED: &str = "015b092ded2eec4838dee20a5d172c428faa863a74e3706cd3aeb6581df0fd16";

/// The tool must answer in a few seconds; the spec's budget is <5s. Generous
/// headroom for CI, but it still catches a pathological RPC round-trip.
const TIME_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sororewind")
}

#[test]
#[ignore = "requires network access to the testnet RPC"]
fn known_failed_tx_replays_and_matches() {
    let started = Instant::now();
    let out = Command::new(bin())
        .arg(KNOWN_FAILED)
        .arg("--no-color")
        .output()
        .expect("failed to spawn sororewind");
    let elapsed = started.elapsed();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let code = out.status.code().unwrap_or(-1);

    assert_eq!(
        code, 0,
        "expected fidelity match (exit 0), got {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("replay matches network"),
        "fidelity line missing\n{stdout}"
    );
    assert!(
        stdout.contains("where it broke"),
        "narrative section missing\n{stdout}"
    );
    assert!(
        elapsed < TIME_BUDGET,
        "replay took {elapsed:?}, budget is {TIME_BUDGET:?}"
    );
}

#[test]
#[ignore = "requires network access to the testnet RPC"]
fn bad_hash_is_a_clean_error() {
    let out = Command::new(bin())
        .arg("not-a-hash")
        .output()
        .expect("failed to spawn sororewind");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("64 hexadecimal characters"),
        "unexpected error text: {stderr}"
    );
}
