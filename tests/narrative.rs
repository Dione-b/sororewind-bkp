//! Snapshot tests over the recorded fixtures.
//!
//! Every directory under `fixtures/` is a complete offline RPC recording of
//! one failed testnet transaction: `hash.txt` names the transaction, the rest
//! of the files are raw JSON-RPC responses. These tests drive the real binary
//! (`--fixture`, `--no-color`) so what is snapshotted is exactly what a user
//! sees, and they never touch the network — the whole point of recording.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const FIXTURE_TIME_BUDGET: Duration = Duration::from_secs(5);

/// Every fixture directory, sorted so snapshot names are stable.
fn fixtures() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", root.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("hash.txt").exists())
        .collect();
    dirs.sort();
    assert!(
        !dirs.is_empty(),
        "no fixtures found under {} — run scripts/record-fixtures.sh",
        root.display()
    );
    dirs
}

fn hash_of(fixture: &Path) -> String {
    std::fs::read_to_string(fixture.join("hash.txt"))
        .expect("hash.txt unreadable")
        .trim()
        .to_string()
}

/// Run the binary against a fixture and return `(exit_code, stdout, stderr)`.
fn run(fixture: &Path, extra: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_sororewind"))
        .arg(hash_of(fixture))
        .args(["--fixture", fixture.to_str().unwrap(), "--no-color"])
        .args(extra)
        .output()
        .expect("failed to spawn sororewind");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Human narrative, with the fixture directory redacted so snapshots do not
/// depend on where the repo lives.
fn human_report(fixture: &Path) -> String {
    let (code, stdout, stderr) = run(fixture, &[]);
    let dir = fixture.to_str().unwrap().to_string();
    let text = format!("--- exit code: {code}\n{stdout}{stderr}");
    text.replace(&dir, "<fixture>")
}

#[test]
fn narratives() {
    for fixture in fixtures() {
        let name = fixture.file_name().unwrap().to_string_lossy().into_owned();
        insta::assert_snapshot!(name, human_report(&fixture));
    }
}

/// Every recorded fixture must still replay-match the network's recorded
/// result. This is the tool's core trust criterion; a fixture that drifts is
/// a fixture that must be re-recorded or dropped.
#[test]
fn every_fixture_replays_clean() {
    for fixture in fixtures() {
        let started = Instant::now();
        let (code, _, _) = run(&fixture, &[]);
        let elapsed = started.elapsed();
        let name = fixture.file_name().unwrap().to_string_lossy();
        assert_eq!(
            code,
            0,
            "fixture {} diverged from its recorded on-chain result \
             (exit {code}; re-record with scripts/record-fixtures.sh)",
            fixture.display()
        );
        assert!(
            elapsed < FIXTURE_TIME_BUDGET,
            "fixture {name} took {elapsed:?}; expected under {FIXTURE_TIME_BUDGET:?}"
        );
    }
}

/// JSON output is stable machine-readable output: parse it, and check the
/// fields a consumer would key on.
#[test]
fn json_output_parses_and_reports_fidelity() {
    for fixture in fixtures() {
        let name = fixture.file_name().unwrap().to_string_lossy().into_owned();
        let (code, stdout, stderr) = run(&fixture, &["--json"]);
        assert_eq!(code, 0, "{name}: exit {code}: {stderr}");
        let v: serde_json::Value =
            serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{name}: bad JSON: {e}"));
        assert_eq!(v["fidelity"]["match"], true, "{name}: {stdout}");
        assert!(v["hash"].as_str().is_some(), "{name}: no hash field");
        assert!(
            v["failure"]["name"].is_string() || v["failure"]["name"].is_null(),
            "{name}: malformed failure.name"
        );
        assert!(
            v["failure"]["nameOrigin"].is_string() || v["failure"]["nameOrigin"].is_null(),
            "{name}: malformed failure.nameOrigin"
        );
    }
}
