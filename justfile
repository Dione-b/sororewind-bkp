# Common tasks. Run `just` to list them; see CONTRIBUTING.md for setup.

# Minimum supported Rust, read from `rust-version` in Cargo.toml.
msrv := `sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml`

default:
    @just --list

# Everything CI enforces on a pull request.
check: fmt clippy test snapshots docs msrv deny

# Formatting, as CI checks it.
fmt:
    cargo fmt --all --check

# Lints, with warnings as errors.
clippy:
    cargo clippy --all-targets --locked -- -D warnings

# The offline test suite.
test:
    cargo test --locked

# No pending or unreferenced snapshots (needs cargo-insta).
snapshots:
    cargo insta test --check --unreferenced=reject

# The API docs, with warnings as errors.
docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked

# Build on the minimum supported Rust.
msrv:
    cargo +{{msrv}} check --locked --all-targets

# Advisories, licences and sources (needs cargo-deny).
deny:
    cargo deny check

# Review pending snapshot changes (needs cargo-insta).
snap:
    cargo insta review

# The live test against testnet, as the nightly CI job runs it.
live:
    cargo test --release --locked --test live -- --ignored --nocapture

# List recent failed testnet transactions, to pick fixture hashes from.
find *args:
    cargo run --locked --example find_failed -- {{args}}

# Record (or refresh) fixture <name> from transaction <hash>.
record name hash *args:
    cargo run --locked --example record_fixture -- {{name}} {{hash}} {{args}}
