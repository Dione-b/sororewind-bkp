# sororewind

Replay a failed Soroban transaction locally and see exactly where, and why, it
broke.

```
$ sororewind --fixture fixtures/contract-error
sororewind  dd1f9b…53db  testnet
ledger 5672381 · protocol 30 · 2026-11-11 08:25:48 UTC

✓ replay matches network
  on-chain  trapped
  local     trapped

where it broke
  contract error #10 · BalanceError
  "resulting balance is not within the allowed range [10000000, -15000000100, 9223372036854775807]"
  in CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC::transfer (depth 0)

call tree
CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC::transfer(GB2YUAICRUQNAHAEAYDA7SS4SGQTECKKCU6PQ7BX32ZRX67VBH7MSCWC, GBZH7YHAQYRG3MQFKFZDOGIKEEYGZ6LJNCEHYOA7K7BDLJGLQI6R3RAG, 20000000000)  ✗
```

A failed transaction on Stellar leaves you a result code and a pile of
diagnostic events. sororewind turns that into an answer: it fetches the
transaction from a Soroban RPC, rebuilds the ledger state it ran against
(footprint entries, TTLs, network cost settings, ledger header), runs it again
with the same `soroban-env-host` the network uses, and narrates the call tree
up to the point of failure.

Then it checks its own work. The local result is compared with the result the
network recorded. When they match, the call tree and error you are reading are
the ones the network executed. When they don't, sororewind tells you so and
shows what changed, rather than guessing.

## Install

Each tagged release publishes prebuilt binaries on the
[Releases page](https://github.com/Dione-b/sororewind/releases): macOS (Apple
silicon and Intel), Linux (x86_64 and arm64, plus a static musl build for
x86_64 that runs on any distribution) and Windows (x86_64). An installer
script puts the right one on your `PATH`:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/Dione-b/sororewind/releases/latest/download/sororewind-installer.sh | sh
```

On Windows, in PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/Dione-b/sororewind/releases/latest/download/sororewind-installer.ps1 | iex"
```

sororewind is not on crates.io yet. To build it from source instead, with
Rust 1.89 or newer:

```bash
cargo install --locked --git https://github.com/Dione-b/sororewind
```

Keep `--locked`: it builds the embedded hosts with the exact dependency
versions they are tested with. Without it, Cargo resolves their dependencies
afresh, and a different version can change what a host charges for a call,
turning a match into a divergence.

Each build replays the Stellar protocols of the Soroban hosts it embeds:
currently 29 and 30, so it covers testnet and mainnet across an upgrade, when
testnet runs the new protocol weeks before mainnet. `sororewind --version`
lists them.

## Quick start

Replay a live transaction by its hash, 64 hex characters with or without
`0x`. Testnet is the default:

```bash
sororewind <TX_HASH>
```

Pick another network, or point at your own RPC:

```bash
sororewind <TX_HASH> --network mainnet
```

```bash
sororewind <TX_HASH> --rpc-url <RPC_URL>
```

If the RPC URL holds an API key, pass it in the `SOROREWIND_RPC_URL`
environment variable instead, which keeps it out of your shell history and
the process list. Either way sororewind only ever shows the scheme and host
of the endpoint (`https://rpc.provider.io/…`), in the report and in errors.
Transient RPC failures (connection errors, timeouts, HTTP 429, 502, 503 and
504) are retried a few times before the run gives up.

The transaction must have failed in the Soroban host, and the RPC must still
have it: RPCs keep a limited history, typically about a week, and the error
tells you the window when a hash is too old.

To try sororewind without the network, replay one of the recordings shipped in
the repository, from a clone:

```bash
git clone https://github.com/Dione-b/sororewind && cd sororewind
```

```bash
cargo run -- --fixture fixtures/contract-error
```

## Reading the output

Every narrative has a header, a fidelity verdict, and a call tree. The other
parts appear when they have something to say, or when you ask for detail:

- **Header.** The transaction hash, the network, and the ledger it ran in, with
  its protocol version and close time.
- **Fidelity.** `✓ replay matches network` means the local run produced the
  same result as the network, so everything below can be trusted.
  `⚠ replay diverged` means it did not; the line says how (a different result
  code, or the same code with a different error). A separate warning appears
  if the network's settings changed since the transaction ran.
- **Where it broke.** The error that ended the transaction: its type (contract,
  auth, storage, budget, …), the contract's own error name when the contract
  declares one, the message the host logged, and the contract function and
  call depth where it was raised.
- **Call tree.** Every contract call in order, with decoded arguments. Nested
  calls hang off `├─` / `└─` rails, `✗` marks the call that failed, and lines
  starting with `·` are the contract's log messages. A call that failed but
  whose caller caught the failure with `try_call` and carried on is marked
  `(recovered by try_call)`; one that never returned because the transaction
  failed inside it is marked `(no return)`.
- **Footprint.** The number of entries the transaction touched, which of them
  changed at or after its ledger (value, TTL, or archived), how many were
  missing, and whether the random-number seed was derived or fixed. It is
  printed when the replay diverges, and always with `-v`.

`-v` adds return values, the CPU and memory budget (local against the limits,
and what the network used), and the full footprint lists. It also prints
debug lines on stderr, tagged with where they come from: `[spec]` when a
contract's error names could not be read, `[replay]` when ledger metadata or
a cached module was unavailable, `[rpc]` for retried requests, and `[record]`
while recording a fixture. `-vv` also prints the raw on-chain and local
diagnostic events, plus a trace of every RPC request and response (`[rpc]`,
or `[fixture]` when replaying a fixture) on stderr. The report itself always
goes to stdout, so capture both to keep everything: `sororewind <TX_HASH> -vv
2>&1 | tee run.log`.

## Exit codes

| Code | Meaning |
|------|---------|
| `0`  | The replay ran and **matched** the result the network recorded. |
| `1`  | sororewind could not replay the transaction, for example: bad hash or flags, unreachable RPC, wrong network, a transaction that succeeded or is not a Soroban transaction, another protocol version, missing state. |
| `2`  | The replay ran but **diverged** from the network. |

`2` is an answer, not a crash: the narrative is printed, but the local run did
not reproduce the network's result, so read it with care. See [Why a replay
can diverge](#why-a-replay-can-diverge). Invalid flags exit `1` rather than
the usual `2`, so a script can treat `2` strictly as "diverged".

## Options

| Option | Description |
|--------|-------------|
| `-n, --network <NETWORK>` | `testnet` (default), `mainnet`, `futurenet`, or `local` (a quickstart or standalone node). Selects the default RPC. |
| `--rpc-url <URL>` | Use this RPC instead of the network's default. Must be an `http://` or `https://` URL; `SOROREWIND_RPC_URL` sets it from the environment. |
| `--json` | Print a JSON document instead of the narrative. |
| `--wasm-debug <PATH>` | For a contract error, read the contract's error names from a local WASM file instead of the ledger. |
| `--color <auto\|always\|never>` | When to colour the narrative. `auto` (the default) colours a terminal, and honours a non-empty `NO_COLOR`, `CLICOLOR_FORCE` and `TERM=dumb`. `always` keeps colour in a pager: `sororewind … --color always \| less -R`. |
| `--no-color` | Same as `--color never`. |
| `-v, --verbose` | Repeat for more detail (`-v`, `-vv`). |
| `--fixture <DIR>` | Replay from a recorded fixture directory, offline. The hash is optional, since the fixture records it; a different hash is an error. Cannot be combined with `--rpc-url`. |
| `--completions <SHELL>` | Print a completion script for `bash`, `zsh`, `fish`, `elvish` or `powershell`, e.g. `sororewind --completions fish > ~/.config/fish/completions/sororewind.fish`. |
| `--man` | Print a man page, e.g. `sororewind --man > ~/.local/share/man/man1/sororewind.1`. |
| `-V, --version` | The version, the protocols this build replays, and the commit and target it was built from. Include it in bug reports. |

The default RPCs are `https://soroban-testnet.stellar.org` (testnet),
`https://mainnet.sorobanrpc.com` (mainnet, run by a third party),
`https://rpc-futurenet.stellar.org` (futurenet) and
`http://localhost:8000/rpc` (local).

When you pass `--network` explicitly, sororewind checks it against the
passphrase the RPC reports and stops with an error if they differ, instead of
producing a mislabelled report.

## JSON output

`--json` prints the full report as one document, whatever the verbosity. Its
main fields:

| Field | Content |
|-------|---------|
| `schemaVersion` | Version of the document's shape; currently `1`. It changes only when a field is removed, renamed, or changes meaning; new fields do not change it. |
| `hash`, `network`, `ledger`, `protocol`, `closeTime` | Which transaction ran, and where. |
| `feeBump` | Whether the transaction was wrapped in a fee-bump; `hash` is then whichever of the outer and inner hashes was asked for. |
| `rpc` | Where the responses came from: the endpoint as scheme and host only (a path or query may hold an API key), or `fixture:<dir>`. |
| `status` | Always `failed`: sororewind only replays failed transactions. |
| `fidelity.match`, `fidelity.divergence` | Whether the replay matched, and if not, how it diverged. |
| `result.onchain`, `result.local` | Each side's result, as `{ code, resultHash }`. |
| `failure` | The error that ended the transaction, with the contract, function and depth it came from; `null` when there is none. `failure.cause` is the contract error a host error was raised in answer to, when there is one. |
| `calls` | The call tree, one entry per call, with arguments, return value and logs. `args` and `returned` are abbreviated for reading; `argsXdr` and `returnedXdr` hold the same values whole, as base64 `ScVal` XDR. |
| `budget`, `footprint`, `prngSeed` | Resource use, the footprint and its drift, and how the random-number seed was obtained. |
| `diagnostics` | The on-chain and local diagnostic events, one line each (`onchain`, `local`) and as base64 `DiagnosticEvent` XDR (`onchainXdr`, `localXdr`). |

Enum-like values are lower `snake_case` throughout (`"status": "failed"`,
`"errorType": "wasm_vm"`, `"reason": "extended_since"`).

Exit codes are the same as for the narrative, so a script can branch on the
exit code and read the details from the document.

When sororewind cannot replay the transaction (exit `1`), `--json` still
prints a document, next to the message on stderr:

```json
{
  "schemaVersion": 1,
  "error": { "kind": "not_found", "message": "transaction … not found on testnet. …" }
}
```

`kind` is stable: `not_found`, `not_a_failure`, `not_soroban`,
`never_reached_host`, `protocol_mismatch`, `network_mismatch`,
`ledger_unavailable`, `rpc_unreachable`, `rpc_error`, and others for
malformed responses and fixtures. Invalid command-line arguments are reported
by the argument parser, as text.

## Why a replay can diverge

A divergence (exit `2`) means the local run did not reproduce what the network
recorded. These are the known causes:

- **The state moved on, and could not be rebuilt.** The RPC only serves the
  *current* value of a ledger entry. Entries written after the transaction
  are rebuilt from the ledger metadata the RPC keeps (`footprint … N rebuilt`,
  listed with `-v`; `footprint.rebuilt` in JSON), which is what lets
  contracts that rewrite the same entry every few ledgers, such as price
  oracles, replay as a match. An entry stays `drifted`, with the reason, when
  the metadata is outside the RPC's retention window (about a week), when the
  RPC keeps none, or when the entry's history is too long to follow. TTL
  extensions and deletions are only seen in the 16 ledgers after the
  transaction; network settings are not rebuilt.
- **Rent priced from a moving window.** The checks stellar-core makes around
  the host are mirrored: archived entries, per-entry size limits, disk-read
  and write bytes, events size, and the refundable fee (`insufficient
  refundable fee`). The rent part of that fee depends on the network's
  state-size window, which is resampled every few minutes; the replay uses
  the current one. That only matters within a few stroops of the limit, and
  the report then lists `LiveSorobanStateSizeWindow` among the changed
  settings.
- **Unknown apply order.** The host's random-number seed is derived the way the
  network derives it, from the ledger's transaction set and the transaction's
  position in it. If the RPC does not report that position, a fixed seed is
  used (`prng seed fixed (apply order unknown)` in the footprint line,
  `"prngSeed": "fixed"` in JSON), and a contract that draws random numbers can
  diverge.

Some transactions cannot be replayed at all. sororewind exits `1` and explains
why:

- **Too old.** The RPC no longer has the transaction or the ledger it ran in.
- **Another protocol version.** Each Soroban host refuses to execute a ledger
  from any protocol but its own, and a build embeds the hosts for the
  previous and the current protocol (`sororewind --version`). A transaction
  from any other protocol is refused; the error names the host version you
  would need.
- **Failed before the host.** A transaction that stellar-core rejected during
  validation or fee processing ran no contract code, so there is nothing to
  replay.

sororewind replays and explains. It does not suggest fixes or profile
contracts.

## Development

Setup, the checks a pull request must pass, and the protocol-upgrade
checklist are in [CONTRIBUTING.md](CONTRIBUTING.md). With `just` installed,
`just check` runs what CI enforces. The individual commands:

```bash
cargo build
```

```bash
cargo test
```

```bash
cargo test --release --test live -- --ignored --nocapture
```

`cargo test` is fully offline: unit tests plus a snapshot of the narrative for
every fixture. The last command is the live test that CI runs nightly: it finds
recent failed transactions on testnet and checks that they replay quickly and
that at least one matches.

CI also runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
the tests on Linux, macOS and Windows, a snapshot check that rejects pending
or unreferenced snapshots, `cargo doc` with warnings as errors, a `cargo
check` on the minimum supported Rust (1.89), and `cargo deny check`.

### How the code is organised

`src/pipeline.rs` runs the stages in order; `src/lib.rs` maps the outcome to
an exit code. Each stage lives in its own module:

| Stage | Module | What it does |
|-------|--------|--------------|
| Fetch | `rpc.rs`, `transport.rs` | Talk to the RPC, or answer from a fixture. |
| Decode | `tx.rs` | Read the envelope, footprint and on-chain result. |
| Reconstruct | `state.rs` | Rebuild ledger entries, TTLs, network settings and the ledger header. |
| Replay | `replay.rs` | Run the host and compare its result with the network's. |
| Narrate | `story.rs`, `spec.rs` | Turn diagnostic events into a call tree and a failure; read error names from the contract's spec. |
| Report | `report.rs`, `render.rs` | Hold the outcome as data (the JSON schema), and print the narrative or the document. |

`discover.rs` finds recent failed transactions on a live network, for the
fixture tooling and the live test.

### Fixtures

A fixture is a complete offline recording of every RPC response one replay
needs. Each directory under `fixtures/` holds one failed transaction, from testnet
unless its name says otherwise (`mainnet-auth-error`);
`tests/narrative.rs` pins its narrative and exit code and checks the main JSON
fields. The layout is documented at the top of `src/transport.rs`.

Fixtures must replay as a match, so they have to be recorded soon after the
transaction ran. List recent failures; the first column is the category, which
is also the usual fixture name:

```bash
cargo run --example find_failed
```

Then record one. An existing name refreshes that fixture, a new name adds one:

```bash
cargo run --example record_fixture -- contract-error <TX_HASH>
```

By default `record_fixture` keeps a recording only if it replays as a match
(`--allow-diverged` keeps a divergence on purpose), and it never replaces a
working fixture with a failed one. Without arguments it re-records every
fixture from the hash and network it was made for, which only works while the RPC still
has those transactions.

A new fixture also needs a line in the `fixtures!` list in
`tests/narrative.rs` and an accepted snapshot: run `cargo insta review`, after
`cargo install cargo-insta`.

Each fixture comes from a `find_failed` category:

| Fixture | How to find one |
|---------|-----------------|
| `auth-error`, `budget-error`, `contract-error`, `storage-error` | A transaction of that category. For `budget-error`, pick one whose result is `resource limit exceeded`. |
| `refundable-fee` | A `budget-error` whose result is `insufficient refundable fee`. |
| `nested-contract-error` | A `contract-error` raised by a WASM contract (the report names it: `contract error #17 · Limit`) whose call tree spans several contracts and levels. |
| `rebuilt-entry` | A failure of a contract that rewrites its entries every few ledgers (an oracle's `write_prices`, for example) whose replay reports `N rebuilt`. |
| `auth-check-auth` | An `auth-error` whose call tree, from `sororewind <TX_HASH>`, shows a `__check_auth` call: a custom account contract rejected the signature. |
| `mainnet-*` | A mainnet transaction, recorded with `--network mainnet`, that keeps the older embedded host covered while mainnet runs the previous protocol. |

Recorded fixtures keep working offline indefinitely. Re-record them when:

- a protocol is dropped from the embedded hosts: its fixtures no longer
  replay. When a host is added, record fixtures for the new protocol and keep
  the others until their host goes;
- the fixture layout changes, which bumps `FIXTURE_VERSION` in
  `src/transport.rs`, and old fixtures fail with "re-record it";
- testnet is reset.

In every case the old hashes are gone from the RPC, so record fresh
transactions found with `find_failed` rather than running `record_fixture`
without arguments.

Error paths that no recording covers are tested by rewriting a fixture's RPC
responses in memory (`common::Overlay`): see `tests/errors.rs` for every error
a user can hit, and `tests/scenarios.rs` for fee-bumps, archived entries and
network mismatches.

## License

Apache-2.0
