//! The minimum supported Rust is written once, as `rust-version` in
//! Cargo.toml; CI and the justfile read it from there. The docs quote it in
//! prose, so this test keeps those copies in step with it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

const MSRV: &str = env!("CARGO_PKG_RUST_VERSION");

fn read(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(name))
        .unwrap_or_else(|e| panic!("reading {name}: {e}"))
}

#[test]
fn the_docs_quote_the_rust_version_from_cargo_toml() {
    let copies = [
        ("README.md", format!("Rust {MSRV} or newer")),
        ("README.md", format!("supported Rust ({MSRV})")),
        ("CONTRIBUTING.md", format!("(currently {MSRV})")),
        (
            "CONTRIBUTING.md",
            format!("rustup toolchain install {MSRV} --profile minimal"),
        ),
    ];
    for (file, expected) in copies {
        assert!(
            read(file).contains(&expected),
            "{file} should say `{expected}`: update it to match `rust-version` in Cargo.toml"
        );
    }
}
