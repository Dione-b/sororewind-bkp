//! Records the commit and the target a binary was built from, for
//! `--version`: a report from a `cargo install --git` build names the exact
//! code it ran.

use std::path::Path;
use std::process::Command;

fn main() {
    // Only this repository's own history counts: a package from crates.io, or
    // one vendored inside another repository, has no commit of its own.
    let git = Path::new(".git");
    let commit = if git.exists() {
        // A new commit or a checkout changes HEAD or the ref it points to.
        println!("cargo:rerun-if-changed=.git/HEAD");
        println!("cargo:rerun-if-changed=.git/refs/heads");
        Command::new("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|commit| commit.trim().to_string())
            .filter(|commit| !commit.is_empty())
    } else {
        println!("cargo:rerun-if-changed=build.rs");
        None
    };
    println!(
        "cargo:rustc-env=SOROREWIND_COMMIT={}",
        commit.as_deref().unwrap_or("unknown")
    );
    println!(
        "cargo:rustc-env=SOROREWIND_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
}
