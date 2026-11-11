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
with warnings as errors, the offline test suite, a build on the minimum
supported Rust, and `cargo deny check`. CI additionally runs the tests on
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
  network has moved to a protocol newer than the one this build embeds.

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

sororewind embeds one `soroban-env-host`, and the host refuses to execute a
ledger from any other protocol. The day a network upgrades, this build stops
replaying its transactions. The nightly `live` job fails when testnet or
mainnet runs a protocol newer than the embedded one; that is the signal to
start.

Testnet upgrades before mainnet. Between the two upgrades, a build serves only
one of them: a build for the new protocol cannot replay mainnet until mainnet
catches up. Keep the previous release available for that window.

The checklist:

1. **Bump the lockstep pair.** In `Cargo.toml`, move `stellar-xdr` and
   `soroban-env-host` to the new protocol's major version together (the host's
   major version is the protocol it executes). Then run:

   ```bash
   cargo update -p soroban-env-host -p stellar-xdr
   ```

   Dependabot deliberately leaves these major versions to you.
2. **Make it build.** New XDR variants show up as non-exhaustive `match`
   errors; decide what each one means for replay rather than adding a
   catch-all. Check `replay.rs` for changes to the `invoke_host_function`
   signature.
3. **Re-check what replay mirrors from stellar-core.** Review each of these
   against the new protocol's CAPs and core release notes. Where the comment
   says when it was last checked, update the date and protocol:
   - the PRNG seed derivation (`replay::prng_seed`);
   - the post-host checks (`PostHostChecks` in `replay.rs`);
   - the network settings read (`CONFIG_SETTINGS` in `state.rs`);
   - the liveness and TTL rules (top of `state.rs`).
4. **Re-record every fixture.** Old fixtures now fail with a protocol
   mismatch. Their hashes are gone from the RPC, so record fresh transactions
   (README, Fixtures), then review the snapshots with `just snap`.
5. **Run `just check` and `just live`.**
6. **Update the docs.** Update protocol mentions in the README, and add the
   upgrade to `CHANGELOG.md`.

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
