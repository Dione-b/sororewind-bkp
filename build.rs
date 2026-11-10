//! Reports the exact `soroban-env-host` version Cargo resolved, so the
//! protocol-guard message can name the crate a reader would have to upgrade.

use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let version = fs::read_to_string("Cargo.lock")
        .ok()
        .and_then(|lock| lock_version(&lock))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=SOROBAN_ENV_HOST_VERSION={version}");
}

fn lock_version(lock: &str) -> Option<String> {
    // Matches `name = "soroban-env-host"` but not `soroban-env-host-*`, since
    // the closing quote is part of the needle.
    let start = lock.find("name = \"soroban-env-host\"")?;
    let rest = &lock[start..];
    let marker = "version = \"";
    let rest = &rest[rest.find(marker)? + marker.len()..];
    Some(rest.split('"').next()?.to_string())
}
