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

### Changed

- The minimum supported Rust is 1.89.
- `getLedgers` fetches one ledger instead of 50 (about 14 MB less per run).
- Missing or malformed network settings and durable entries without a TTL are
  errors instead of silent fallbacks.
- Invalid command-line flags exit with code 1, so `2` always means
  "diverged".

### Fixed

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
