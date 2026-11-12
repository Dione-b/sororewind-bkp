//! Find recent failed Soroban transactions on a live network.
//!
//! Recorded fixtures and any hash written into a test expire with the RPC's
//! retention window (about a week), and testnet is reset periodically. This
//! module lets the live test and the fixture tooling ask the network for
//! fresh material instead of depending on a hash committed to the repository.

use stellar_xdr::InvokeHostFunctionResult;

use crate::display;
use crate::error::{Result, RewindError};
use crate::hash::TxHash;
use crate::rpc::{RpcClient, TxStatus};
use crate::tx;

/// A failed `InvokeHostFunction` transaction found on the network.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The transaction's hash, ready to pass to sororewind.
    pub hash: TxHash,
    /// Sequence number of the ledger it was applied in.
    pub ledger: u32,
    /// The result code the network recorded.
    pub result: InvokeHostFunctionResult,
    /// A coarse label for the kind of failure, e.g. `contract-error`,
    /// `auth-error`, `budget-error`: the last error the network's diagnostics
    /// recorded, or the result code when it kept none.
    pub category: String,
}

/// How far back to look, and how much to bring back.
#[derive(Debug, Clone, Copy)]
pub struct Search {
    /// Stop after this many candidates.
    pub limit: usize,
    /// Scan at most this many ledgers back from the latest one.
    pub max_ledgers: u32,
}

/// Collect failed Soroban invocations, most recent first.
///
/// Recent transactions are preferred because the replay runs against current
/// state: the less time has passed, the less the ledger has drifted.
pub async fn recent_failures(rpc: &RpcClient, search: Search) -> Result<Vec<Candidate>> {
    /// Ledgers scanned in the first step; at testnet volume, a few pages.
    const WINDOW: u32 = 60;
    /// Each step scans this many times the ledgers of the one before. A step
    /// ends on a page that runs into ledgers already scanned, so fewer, larger
    /// steps refetch less; starting small still lets a short search stop early.
    const GROWTH: u32 = 2;

    let health = rpc.health().await?;
    // A health response without its ledger range cannot bound the scan; "no
    // failures found" would be a guess.
    let latest =
        health
            .latest_ledger
            .filter(|latest| *latest > 0)
            .ok_or(RewindError::MissingField {
                method: "getHealth",
                field: "latestLedger",
            })?;
    let floor = latest
        .saturating_sub(search.max_ledgers)
        .max(health.oldest_ledger.unwrap_or(0));

    let mut found = Vec::new();
    // The windows are `[lower, upper)`, and the latest ledger is scanned too.
    let mut upper = latest.saturating_add(1);
    let mut window = WINDOW;
    while upper > floor && found.len() < search.limit {
        let lower = upper.saturating_sub(window).max(floor);
        let mut failures = match failures_between(rpc, lower, upper).await {
            Ok(failures) => failures,
            // The RPC pruned its oldest ledgers while we were scanning: what
            // was found so far is still good.
            Err(err) if err.is_ledger_out_of_range() => {
                log::debug!(target: "rpc", "stopping the scan at ledger {upper}: {err}");
                break;
            }
            Err(err) => return Err(err),
        };
        failures.reverse();
        found.extend(failures);
        upper = lower;
        window = window.saturating_mul(GROWTH);
    }
    found.truncate(search.limit);
    Ok(found)
}

/// Failed Soroban invocations in ledgers `[from, to)`, oldest first.
async fn failures_between(rpc: &RpcClient, from: u32, to: u32) -> Result<Vec<Candidate>> {
    let mut found = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = rpc.get_transactions(from, cursor.as_deref()).await?;
        let mut past_the_end = page.transactions.is_empty();
        for raw in &page.transactions {
            if raw.ledger.is_some_and(|ledger| ledger >= to) {
                past_the_end = true;
                break;
            }
            if raw.status != TxStatus::Failed {
                continue;
            }
            let Some(hash) = raw
                .tx_hash
                .as_deref()
                .and_then(|h| h.parse::<TxHash>().ok())
            else {
                continue;
            };
            // Anything that is not a host invocation fails to decode; skip it.
            if let Ok(tx) = tx::decode(&hash, raw) {
                found.push(Candidate {
                    category: category(&tx),
                    hash,
                    ledger: tx.ledger,
                    result: tx.onchain_result,
                });
            }
        }
        cursor = page.cursor;
        if past_the_end || cursor.is_none() {
            return Ok(found);
        }
    }
}

fn category(tx: &tx::DecodedTx) -> String {
    let last_error = tx.onchain_diagnostic_events.iter().rev().find_map(|event| {
        match display::event_parts(&event.event).0 {
            [
                stellar_xdr::ScVal::Symbol(kind),
                stellar_xdr::ScVal::Error(error),
                ..,
            ] if kind.0.as_slice() == b"error" => Some(error.discriminant()),
            _ => None,
        }
    });
    match last_error {
        Some(error_type) => format!(
            "{}-error",
            display::error_type_name(error_type).replace(' ', "-")
        ),
        None => display::result_code(&tx.onchain_result).replace('_', "-"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec;
    use crate::test_support::{
        InvokeSpec, MemoryTransport, error_event, invoke_envelope, result_xdr,
    };
    use serde_json::json;
    use stellar_xdr::{ScError, ScErrorCode};

    fn tx_json(hash_byte: &str, ledger: u32, status: &str) -> serde_json::Value {
        let envelope = invoke_envelope(&InvokeSpec::default());
        json!({
            "status": status,
            "txHash": hash_byte.repeat(32),
            "ledger": ledger,
            "applicationOrder": 1,
            "envelopeXdr": codec::b64_encode(&codec::encode(&envelope, "e").unwrap()),
            "resultXdr": result_xdr(InvokeHostFunctionResult::Trapped),
        })
    }

    #[tokio::test]
    async fn finds_recent_failures_newest_first() {
        let transport = MemoryTransport::new()
            .on(
                "getHealth",
                &json!({"result": {"latestLedger": 1000, "oldestLedger": 900}}),
            )
            .on_fn("getTransactions", |params| {
                // One page per window, then an empty page.
                if params["pagination"]["cursor"].is_string() {
                    return json!({"result": {"transactions": [], "cursor": "end"}});
                }
                let start = params["startLedger"].as_u64().unwrap() as u32;
                json!({"result": {"cursor": "next", "transactions": [
                    tx_json("aa", start, "SUCCESS"),
                    tx_json("bb", start + 1, "FAILED"),
                    tx_json("cc", start + 2, "FAILED"),
                ]}})
            });
        let rpc = RpcClient::new(transport);

        let found = recent_failures(
            &rpc,
            Search {
                limit: 3,
                max_ledgers: 500,
            },
        )
        .await
        .unwrap();
        let ledgers: Vec<u32> = found.iter().map(|c| c.ledger).collect();
        // Newest window first, newest transaction first within it. The first
        // window is the 60 ledgers up to and including the latest.
        assert_eq!(ledgers, [943, 942, 902]);
        assert!(
            found
                .iter()
                .all(|c| c.result == InvokeHostFunctionResult::Trapped)
        );
        assert_eq!(found[0].category, "trapped");
    }

    #[tokio::test]
    async fn stops_at_the_retention_floor() {
        let transport = MemoryTransport::new()
            .on(
                "getHealth",
                &json!({"result": {"latestLedger": 1000, "oldestLedger": 990}}),
            )
            .on("getTransactions", &json!({"result": {"transactions": []}}));
        let calls = transport.calls();
        let rpc = RpcClient::new(transport);
        let found = recent_failures(
            &rpc,
            Search {
                limit: 5,
                max_ledgers: 10_000,
            },
        )
        .await
        .unwrap();
        assert!(found.is_empty());
        let calls = calls.lock().unwrap();
        let starts: Vec<u64> = calls
            .iter()
            .filter(|(method, _)| method == "getTransactions")
            .map(|(_, params)| params["startLedger"].as_u64().unwrap())
            .collect();
        assert_eq!(
            starts,
            [990],
            "must not ask for ledgers the RPC no longer has"
        );
    }

    fn search() -> Search {
        Search {
            limit: 10,
            max_ledgers: 100,
        }
    }

    #[tokio::test]
    async fn the_latest_ledger_is_scanned_too() {
        let transport = MemoryTransport::new()
            .on(
                "getHealth",
                &json!({"result": {"latestLedger": 1000, "oldestLedger": 900}}),
            )
            .on_fn("getTransactions", |params| {
                if params["pagination"]["cursor"].is_string() {
                    return json!({"result": {"transactions": []}});
                }
                json!({"result": {"cursor": "next", "transactions": [
                    tx_json("bb", 1000, "FAILED"),
                    // A status from a newer RPC is skipped, not fatal.
                    tx_json("cc", 1000, "SOMETHING_NEW"),
                ]}})
            });
        let found = recent_failures(&RpcClient::new(transport), search())
            .await
            .unwrap();
        assert!(found.iter().any(|c| c.ledger == 1000), "{found:?}");
        assert!(found.iter().all(|c| c.hash.as_str().starts_with("bb")));
    }

    #[tokio::test]
    async fn a_health_response_without_ledgers_is_an_error() {
        for health in [json!({"status": "healthy"}), json!({"latestLedger": 0})] {
            let transport = MemoryTransport::new().on("getHealth", &json!({ "result": health }));
            let err = recent_failures(&RpcClient::new(transport), search())
                .await
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                "the getHealth response is missing `latestLedger`"
            );
        }
    }

    #[tokio::test]
    async fn a_floor_pruned_during_the_scan_keeps_what_was_found() {
        let transport = MemoryTransport::new()
            .on(
                "getHealth",
                &json!({"result": {"latestLedger": 1000, "oldestLedger": 100}}),
            )
            .on_fn("getTransactions", |params| {
                if params["pagination"]["cursor"].is_string() {
                    return json!({"result": {"transactions": []}});
                }
                let start = params["startLedger"].as_u64().unwrap() as u32;
                if start < 900 {
                    return json!({"error": {"code": -32600, "message": format!(
                        "start ledger ({start}) must be between the oldest ledger: 900 and \
                         the latest ledger: 1000 for this rpc instance"
                    )}});
                }
                json!({"result": {"cursor": "next", "transactions": [
                    tx_json("bb", start, "FAILED"),
                ]}})
            });
        let found = recent_failures(
            &RpcClient::new(transport),
            Search {
                limit: 10,
                max_ledgers: 5_000,
            },
        )
        .await
        .unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[tokio::test]
    async fn windows_grow_so_fewer_pages_are_refetched() {
        let transport = MemoryTransport::new()
            .on(
                "getHealth",
                &json!({"result": {"latestLedger": 10_000, "oldestLedger": 1}}),
            )
            .on("getTransactions", &json!({"result": {"transactions": []}}));
        let calls = transport.calls();
        recent_failures(
            &RpcClient::new(transport),
            Search {
                limit: 1,
                max_ledgers: 1_000,
            },
        )
        .await
        .unwrap();
        let starts: Vec<u64> = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "getTransactions")
            .map(|(_, params)| params["startLedger"].as_u64().unwrap())
            .collect();
        // 60, 120, 240, 480 ledgers, then what is left down to the floor.
        assert_eq!(starts, [9_941, 9_821, 9_581, 9_101, 9_000]);
    }

    #[test]
    fn categorises_by_the_last_recorded_error() {
        let mut tx = crate::test_support::decoded_tx();
        assert_eq!(category(&tx), "trapped");
        tx.onchain_diagnostic_events = vec![
            error_event(None, ScError::Contract(1), "inner"),
            error_event(None, ScError::WasmVm(ScErrorCode::InvalidAction), "outer"),
        ];
        assert_eq!(category(&tx), "wasm-vm-error");
    }
}
