# Changelog

All notable changes to sororewind are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/). The JSON document and the exit
codes are part of the public interface: changes to them are listed here, and
an incompatible change to the JSON bumps its `schemaVersion`.

## [Unreleased]

### Added

- Protocols 29 and 30 in one build: sororewind embeds both Soroban hosts and
  replays each transaction with the one for its ledger's protocol, so testnet
  and mainnet are covered across an upgrade. `--version` lists them.
- Fee-bumped Soroban transactions are replayed; the header and JSON say so.
- `--network futurenet` and `--network local`. An explicit `--network` is
  checked against the RPC's passphrase.
- The random-number seed is derived the way stellar-core derives it, so
  contracts that draw random numbers can replay as a match.
- The footprint report separates value drift, TTL drift, archived entries and
  network-settings drift.
- `--json` documents carry `schemaVersion: 1`.
- Entries that changed since the transaction are rebuilt from ledger metadata
  (`getLedgers`), so busy contracts such as price oracles replay as a match.
  The report lists them (`footprint.rebuilt`) and says why any could not be
  rebuilt (`footprint.rebuildNote`).
- stellar-core's refundable-fee check (`insufficient refundable fee`), the
  disk-read byte limit and the per-entry size limits are mirrored.
- New `footprint` fields in JSON: `rebuilt`, `rebuildNote`, `archivedInferred`,
  `driftUnknown`, `extMissing`, `configDriftUnknown`; and the TTL drift reason
  `restoredSince`.
- `sororewind --version` shows the protocols and host versions it replays.

- A host error raised in answer to a contract error (a custom account's
  `__check_auth` rejecting a signature, for one) reports that contract error
  as its cause, named from the contract's spec (`failure.cause` in JSON).
- Stellar Asset Contract errors are named (`#10` is `BalanceError`).
- `error` and `log` diagnostics keep their arguments: `message [arg, arg]`.

- `--color <auto|always|never>`; `auto` honours `CLICOLOR_FORCE` and
  `TERM=dumb`. `--no-color` stays as an alias of `--color never`.
- In `--json` mode a failed run prints `{"schemaVersion", "error": {"kind",
  "message"}}` on stdout, with a stable `kind` per failure.
- JSON carries values whole next to their display strings: `calls[].argsXdr`,
  `calls[].returnedXdr`, `diagnostics.onchainXdr` and `diagnostics.localXdr`.
- Open frames are marked `(no return)` and recovered ones `(recovered by
  try_call)` at every verbosity, and the call tree is drawn with real
  connectors, so it reads the same without colour.
- Muxed addresses are shown as `M…` strkeys and trustlines with their asset.
- Vectors and maps show their first 8 items and how many more there are.

- Transient RPC failures (connect errors, timeouts, HTTP 429/502/503/504) are
  retried with backoff, honouring `Retry-After`; connecting has its own
  10-second timeout.
- `SOROREWIND_RPC_URL` names the RPC endpoint when `--rpc-url` is not given.
- Prebuilt binaries for each release, including a static Linux (musl) build,
  and a PowerShell installer for Windows.
- `--completions <SHELL>` prints a completion script for bash, zsh, fish,
  elvish or PowerShell, and `--man` prints a man page.
- `--version` names the commit and the target the binary was built from.

### Changed

- The RPC endpoint is shown as scheme and host only, in `rpc` (JSON) and in
  error messages: a path, query or userinfo may hold an API key.
- RPC errors show the JSON-RPC code and the whole cause chain (DNS, TLS,
  connection refused), and read a JSON-RPC error sent with a non-2xx status.
- JSON enum-like values are `snake_case`: `status` is `failed`, `errorType`
  `wasm_vm`, TTL drift reasons `extended_since`, `archived_since`,
  `restored_since`. `failure.depth` is `null` when no frame was open.
- `failure.nameOrigin` is only set when a name was found, and says where the
  spec was read from (the ledger or `--wasm-debug`). A code declared by
  several error enums lists every candidate.

- The minimum supported Rust is 1.89.
- `getLedgers` fetches one ledger instead of 50 (about 14 MB less per run).
- The ledger header and the footprint entries are fetched concurrently, and
  footprint entries and contract code are encoded and copied once.
- Missing or malformed network settings and durable entries without a TTL are
  errors instead of silent fallbacks.
- Invalid command-line flags exit with code 1, so `2` always means
  "diverged".
- The crate uses the 2024 edition.
- Releases are built only once the pull-request checks pass on the tagged
  commit. `cargo install` builds with thin LTO; the published binaries keep
  full LTO.

### Fixed

- `--record` refuses a target that exists and is not a fixture (a file, or a
  non-empty directory without `fixture.json`) instead of replacing it, and
  puts back a fixture an interrupted recording left only in its backup.
- A rate limit or gateway error (429, 502, 503, 504) is retried even when the
  provider sends it as a JSON-RPC error.
- The "did you mean http://…?" hint for an invalid `SOROREWIND_RPC_URL` shows
  only the scheme, host and port.
- `--record` stages the recording and moves it into place in one step once
  the replay has run: an existing fixture is replaced whole or left alone,
  never mixed with another transaction's files, and files are written
  atomically.
- `record_fixture` no longer collides on, or leaves behind, a visible backup
  directory; it keeps the real I/O error, skips stray directories, and can
  re-record a fixture made on a custom network (with `--rpc-url`).
- `find_failed` checks the RPC's passphrase against `--network`.
- `--fixture <dir> <hash>` with a hash the fixture was not recorded for says
  so; a mistyped `--fixture` path is reported as a missing directory instead
  of "recorded for a different transaction".
- Writing to a closed pipe (`sororewind … | head -1`) no longer panics with
  exit code 101.
- `--rpc-url` must be an http(s) URL and `--wasm-debug` an existing file,
  checked when the arguments are parsed; `--help` lists the default RPCs the
  code uses.
- A missing hash, an output failure and a short retention window ("about 0
  days") have fitting messages.
- A ledger outside the RPC's window is reported as such instead of as a raw
  RPC error.
- A transaction status this build does not know no longer makes the whole
  response unparseable; `find_failed` skips such transactions.
- `find_failed` scans the latest ledger too, keeps what it found when the
  RPC prunes its oldest ledgers mid-scan, refetches fewer pages, and fails
  when `getHealth` does not report the latest ledger instead of finding
  nothing.
- Text chosen by a contract, an RPC or a file (logs, error names, function
  names, custom network names) has its control and text-reordering characters
  escaped before it is printed: it can no longer drive the terminal or forge
  lines of the report.
- An empty `NO_COLOR` no longer disables colour.
- The failure is attributed to the last escalation, not to an earlier error
  with the same code that a `try_call` recovered from.
- The call tree no longer nests later calls under a dead frame after a
  recovered self-call.
- A transaction in the ledger that upgraded the protocol or the base reserve
  is replayed under the previous values, as the network ran it.
- A host failure while decoding its inputs is replayed as `trapped` or
  `resource limit exceeded` instead of aborting the run.
- Only entries actually found archived are restored; an archived contract's
  code being restored is no longer pre-parsed for free.
- An entry listed for restoration and restored by a later transaction is
  replayed as archived-and-restored and reported as TTL drift.
- `ENTRY_ARCHIVED` blames the entry the network named; a liveness taken from
  the network's result is reported as inferred.
- Missing `lastModifiedLedgerSeq` and `extXdr` are reported instead of being
  read as "not drifted" and "no extension". The close time comes from the
  ledger header.
- A contract error named from code that changed after the transaction says so.

- The invoker of a host call is the operation's source account when it has
  one.
- After a recovered `try_call` failure, return values are attributed to the
  right frame.
- The protocol-mismatch error names the host version the transaction needs.
