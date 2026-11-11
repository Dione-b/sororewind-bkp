//! Find recent failed Soroban transactions on a live network.
//!
//! Recorded fixtures and any hash written into a test expire with the RPC's
//! retention window (about a week), and testnet is reset periodically. This
//! module lets the live test and the fixture tooling ask the network for
//! fresh material instead of depending on a hash committed to the repository.

use stellar_xdr::InvokeHostFunctionResult;

use crate::display;
use crate::error::Result;
use crate::hash::TxHash;
use crate::rpc::{RpcClient, TxStatus};
use crate::tx;

/// A failed `InvokeHostFunction` transaction found on the network.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub hash: TxHash,
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
    /// Ledgers scanned per step; at testnet volume one step is a few pages.
    const WINDOW: u32 = 60;

    let health = rpc.health().await?;
    let floor = health
        .latest_ledger
        .saturating_sub(search.max_ledgers)
        .max(health.oldest_ledger);

    let mut found = Vec::new();
    let mut upper = health.latest_ledger;
    while upper > floor && found.len() < search.limit {
        let lower = upper.saturating_sub(WINDOW).max(floor);
        let mut window = failures_between(rpc, lower, upper).await?;
        window.reverse();
        found.extend(window);
        upper = lower;
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
            [stellar_xdr::ScVal::Symbol(kind), stellar_xdr::ScVal::Error(error), ..]
                if kind.0.as_slice() == b"error" =>
            {
                Some(error.discriminant())
            }
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
        error_event, invoke_envelope, result_xdr, InvokeSpec, MemoryTransport,
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
        // Newest window first, newest transaction first within it.
        assert_eq!(ledgers, [942, 941, 902]);
        assert!(found
            .iter()
            .all(|c| c.result == InvokeHostFunctionResult::Trapped));
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
