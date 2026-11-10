# sororewind

Replay a failed Soroban transaction locally and narrate the call tree to the
point of failure.

```
$ sororewind 015b092ded2eec4838dee20a5d172c428faa863a74e3706cd3aeb6581df0fd16
sororewind  015b09…fd16  testnet
ledger 5659599 · protocol 30 · 2026-11-10 14:40:38 UTC

✓ replay matches network
  on-chain  trapped
  local     trapped

where it broke
  auth error InvalidAction
  "Unauthorized function call for address"
  in CD4KFB23CQLJ47RIPESVBTQ444R75M6QVEZULWXHHFYYZT7SS2VGXOPW::write_prices (depth 0)

call tree
CD4KFB23CQLJ47RIPESVBTQ444R75M6QVEZULWXHHFYYZT7SS2VGXOPW::write_prices(GBV54BUA…ZTFB, ["ETH"], 0x455448…(335 bytes))  ✗
    · VM call trapped with HostError
```

sororewind fetches the transaction from a Soroban RPC, rebuilds the ledger
state it ran against (footprint entries, TTLs, network cost parameters,
ledger header), re-executes it with the **same `soroban-env-host` version the
network runs**, and prints what happened. When the local outcome matches the
on-chain `resultXdr` you know the narration is telling the truth; when it
does not, sororewind says so instead of guessing.

## Install

```
cargo install sororewind
```

or grab a prebuilt binary from the [releases page](https://github.com/example/sororewind/releases).

## Usage

```
sororewind <TX_HASH> [OPTIONS]

Arguments:
  <TX_HASH>                  Hash of the failed transaction (hex, with or without 0x)

Options:
  -n, --network <NETWORK>    testnet (default) or mainnet
      --rpc-url <URL>        Override the RPC endpoint
      --json                 Machine-readable JSON instead of the human narrative
      --wasm-debug <PATH>    Load contract code from a local WASM file (debugging aid)
      --no-color             Disable ANSI colour (also auto-detected from TTY)
  -v, --verbose...           -v adds budget/footprint detail, -vv adds raw diagnostics
      --fixture <DIR>        Replay from a recorded RPC fixture directory (offline)
      --record <DIR>         Write every RPC response into DIR while running (hidden)
```

### Exit codes

| code | meaning |
|------|---------|
| `0`  | replay ran and **matched** the network's recorded result |
| `1`  | tool error: bad hash, unreachable RPC, non-Soroban tx, protocol mismatch, un-replayable state |
| `2`  | replay ran but **diverged** from the network — the state under us has moved |

`2` is a real answer, not a crash: it means the ledger changed between the
transaction and now (an entry was written or archived since), so the local
replay is running against different state than the network saw.

## What it can and cannot tell you

**Trust the output when it says `✓ replay matches network`.** The fidelity
check compares the local result against the on-chain `resultXdr`; a match
means the call tree, arguments, and error you are reading are the ones the
network actually executed.

Limitations, in rough order of how often they matter:

- **State drift.** The RPC only serves *current* entry state. If any footprint
  entry changed after the transaction ran, replay may diverge (exit `2`).
  sororewind reports exactly which entries drifted.
- **PRNG seed.** The network seeds the host PRNG from the transaction-set
  hash, which is not retrievable from the RPC. sororewind uses a fixed seed.
  A contract that draws random numbers will diverge; the fidelity check is
  what surfaces that.
- **Core-level fee failures.** A transaction rejected by stellar-core's
  refundable-fee/storage-rent check (`insufficient refundable fee`) never
  reaches the host on-chain, so host replay cannot reproduce it. sororewind
  will report divergence and show what the host *would* have done.
- **Retention window.** The RPC keeps ~7 days of history (`120960` ledgers on
  testnet). Older transactions are gone.
- **Protocol lockstep.** `soroban-env-host` refuses to execute a ledger whose
  protocol differs from its own, so this build replays protocol-30
  transactions only; it tells you which host version it embedded and which
  one you need.
- **No fix suggestions, no profiling, no GUI** — this is a replay tool.

## Fixtures

`fixtures/` holds recorded RPC responses for five failed testnet transactions:
contract error, two authorization paths, budget limit, and a storage/footprint
error. The current set does not yet include a panic or an archived-entry
failure. Fixtures exist because the RPC forgets:

- `scripts/record-fixtures.sh` re-records a fixture from the network.
- `tests/narrative.rs` drives the real binary against `--fixture` and
  snapshots the narrative; every fixture must still replay-match (exit 0).

The recording script stages each capture before replacing the old directory,
so an RPC or decode error does not destroy a usable fixture. Fixture recordings
are testnet-only. They expire with the RPC's retention window (~7 days); the
nightly CI job checks one live transaction, but does not generate replacement
failure transactions. Refresh hashes in `fixtures/MANIFEST` when they age out.

The offline replay acceptance test enforces a five-second limit per fixture.
The live RPC check has a wider timeout because network latency is outside the
local replay budget.

## Development

```
cargo build
cargo test              # unit + fixture snapshot tests, fully offline
cargo test -- --ignored # nightly: live-RPC end-to-end test
```

Pipeline stages, one module each:

```
fetch (rpc.rs) → decode (tx.rs) → reconstruct (state.rs) → replay (replay.rs)
      → narrate (story.rs) → render (render.rs)
```

`pipeline.rs` wires them together and owns the exit codes.

## License

Apache-2.0
