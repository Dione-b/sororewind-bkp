# Contributing to sororewind

Thanks for helping. This page covers setting up, the checks a pull request
must pass, and the two maintenance jobs that come up regularly: refreshing
fixtures and upgrading to a new Stellar protocol.

## Setup

You need `rustup`. The repository pins its toolchain in
`rust-toolchain.toml`, so the first `cargo` command in a fresh clone installs
the right compiler, `rustfmt` and `clippy` by itself. If your rustup is older
than 1.28 and does not, run:

```bash
rustup toolchain install
```

The pinned toolchain is what development and CI use. Separately, the code must
still build on the minimum supported Rust, `rust-version` in `Cargo.toml`
(currently 1.89):

```bash
rustup toolchain install 1.89 --profile minimal
```

The common tasks are in a `justfile`. Install the tools it calls once:

```bash
cargo install --locked just cargo-insta cargo-deny
```

## Before you open a pull request

```bash
just check
```

This runs what CI enforces on every pull request: `cargo fmt --check`, clippy
with warnings as errors, the offline test suite, the snapshot check (no
pending or unreferenced snapshots), the API docs with warnings as errors, a
build on the minimum supported Rust, and `cargo deny check`. CI additionally runs the tests on
macOS and Windows.

Lint settings live in `Cargo.toml` (`[lints]`): `unsafe_code` is forbidden,
and `unwrap`/`expect` are warned about outside tests. If an `expect` is truly
unreachable, say why in a comment next to it.

Run `just` to see the other tasks.

## Tests and snapshots

- `just test` is fully offline and takes a few seconds:
  - unit tests live next to the code;
  - `tests/narrative.rs` replays every fixture and snapshots its narrative;
  - `tests/errors.rs` covers every error a user can hit;
  - `tests/scenarios.rs` covers situations no recording does.
- When you change the output on purpose, the snapshot tests fail and write
  `.snap.new` files. Review and accept them with `just snap`, and commit the
  updated `.snap` files with your change.
- `just live` runs the live tests against the public RPCs, as the nightly CI
  job does. They check that recent testnet failures replay, and that no
  network has moved to a protocol newer than every host this build embeds.

To test a situation no fixture covers, rewrite a fixture's RPC responses in
memory with `common::Overlay` instead of recording a new one. See
`tests/errors.rs` for examples.

## Fixtures

How to find, record and refresh fixtures, and when they must be re-recorded,
is in the README under [Fixtures](README.md#fixtures). The short version:

```bash
just find
```

```bash
just record contract-error <TX_HASH>
```

## Protocol upgrades

Each `soroban-env-host` refuses to execute a ledger from any protocol but its
own. sororewind therefore embeds two hosts, for the previous and the current
protocol, the way stellar-core does across an upgrade: testnet moves to a new
protocol weeks before mainnet, and one build covers both. Each transaction is
replayed by the host for its ledger's protocol (`src/host/`).

The nightly `live` job fails when testnet or mainnet runs a protocol newer
than every embedded host; that is the signal to start. The checklist, for a
new protocol N:

1. **Add host N.** In `Cargo.toml`, add
   `soroban-env-host-pN = { package = "soroban-env-host", version = "=N.x.y" }`,
   pinned exactly. Copy `src/host/p<N-1>.rs` to `src/host/pN.rs`, change its
   `use` line, and add it to `mod` and `HOSTS` in `src/host/mod.rs`.
2. **Drop the oldest host**, so at most two remain: remove its `pNN.rs`, its
   `HOSTS` entry and its `Cargo.toml` line. Keep it a little longer only if
   mainnet has not upgraded yet.
3. **Move `stellar-xdr` to the one host N pins.** The rest of the crate uses
   only the newest XDR. The host's major version is not always the XDR's: host
   29 pins `stellar-xdr` 28. Find the right one with
   `cargo tree -p soroban-env-common@N.x.y -e normal --depth 1`. Dependabot
   deliberately leaves these versions to you.
4. **Make it build.** New XDR variants show up as non-exhaustive `match`
   errors; decide what each one means for replay rather than adding a
   catch-all. If the new host changes an API that `src/host/proto.rs` uses,
   adapt it there for every host, behind a small per-host shim if needed.
5. **Re-check what replay mirrors from stellar-core**, against the new
   protocol's CAPs and core release notes. Where a comment says when it was
   last checked, update the date and protocol. If a rule differs between the
   two embedded protocols, gate it on `ledger.protocol` instead of changing it
   for both. What to review:
   - the PRNG seed derivation (`replay::prng_seed`);
   - the checks around the host (`footprint_checks`, `PostHostChecks` and
     `refundable_fee_check` in `replay.rs`) and the fee computation
     (`fees` in `src/host/proto.rs`);
   - how a host failure before execution is classified (`src/host/proto.rs`);
   - the order of ledger entry changes in `LedgerCloseMeta` (`history.rs`);
   - the network settings read (`CONFIG_SETTINGS` in `state.rs`);
   - the liveness and TTL rules (top of `state.rs`).
6. **Fixtures.** Record fixtures for protocol N on testnet (README,
   Fixtures). Keep the previous protocol's fixtures while its host stays,
   including a `mainnet-*` one while mainnet runs it, and remove those of a
   dropped host. Review the snapshots with `just snap`.
7. **Run `just check` and `just live`.**
8. **Update the docs.** Update the protocol mentions in the README, and add
   the upgrade to `CHANGELOG.md`.

The two hosts are linked in one ordinary Cargo build, so their shared
dependencies resolve to one version each. stellar-core does not do this in
production builds, because a different transitive version could change what
the host charges. For a replay tool the fidelity check catches that: a cost
difference that matters shows up as a divergence, not as a wrong answer.

## Releases

Releases are built by [cargo-dist](https://opensource.axo.dev/cargo-dist/)
(`.github/workflows/release.yml`, generated; do not edit it by hand) when a
version tag such as `v0.2.0` is pushed. The release notes come from the
matching section of `CHANGELOG.md`, so move the `Unreleased` entries under the
new version heading before tagging.

## Pull requests

Keep a pull request to one change, and say in the description how you checked
it. User-visible changes get a line in `CHANGELOG.md` under `Unreleased`.
Bug reports are most useful with the transaction hash, the network, and the
`-vv` output.
