//! fetch: the typed Soroban RPC client.
//!
//! Everything here is deliberately dumb: no XDR decoding, no interpretation.
//! The client turns method calls into JSON-RPC requests, unwraps the JSON-RPC
//! envelope, and turns RPC-level failures into messages a human can act on.
//! Where the bytes come from — a live endpoint or a recorded fixture — is the
//! [`Transport`]'s business, not this module's.

use std::collections::HashMap;

use futures_util::{StreamExt, TryStreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Result, RewindError};
use crate::hash::TxHash;
use crate::transport::{Transport, truncate};

/// The most ledger keys one `getLedgerEntries` call accepts.
///
/// <https://developers.stellar.org/docs/data/apis/rpc/api-reference/methods/getLedgerEntries>
pub const MAX_KEYS_PER_CALL: usize = 200;

/// How many `getLedgerEntries` calls run at once.
const MAX_CONCURRENT_CALLS: usize = 4;

/// The most transactions one `getTransactions` page returns.
pub const MAX_TRANSACTIONS_PER_PAGE: u32 = 200;

/// `getTransaction`'s verdict on a hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TxStatus {
    /// The transaction was applied and succeeded.
    Success,
    /// The transaction was applied and failed; its fee was still charged.
    Failed,
    /// The RPC has no record of the hash: never applied, or older than its
    /// retention window.
    NotFound,
    /// A status this build does not know: a newer RPC, or a provider's own.
    #[serde(other)]
    Unknown,
}

/// The subset of a `getTransaction` / `getTransactions` record we care about.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawTransaction {
    /// `getTransaction`'s verdict on the hash.
    pub status: TxStatus,
    /// The transaction's hash, lowercase hex; `None` when the record omits it.
    #[serde(default)]
    pub tx_hash: Option<String>,
    /// Sequence of the ledger the transaction was applied in; `None` for
    /// `NOT_FOUND`.
    #[serde(default)]
    pub ledger: Option<u32>,
    /// One-based position of the transaction in the ledger's apply order.
    #[serde(default)]
    pub application_order: Option<u32>,
    /// The `TransactionEnvelope`, base64 XDR; `None` for `NOT_FOUND`.
    #[serde(default)]
    pub envelope_xdr: Option<String>,
    /// The `TransactionResult`, base64 XDR; `None` for `NOT_FOUND`.
    #[serde(default)]
    pub result_xdr: Option<String>,
    /// The `TransactionMeta`, base64 XDR: the ledger changes and diagnostic
    /// events. `None` for `NOT_FOUND`.
    #[serde(default)]
    pub result_meta_xdr: Option<String>,
}

/// One entry from `getLedgerEntries`, still in pieces; `codec::assemble_entry`
/// puts it back together.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawLedgerEntry {
    /// The `LedgerKey` this entry answers, base64 XDR.
    pub key: String,
    /// The entry body, a `LedgerEntryData`, base64 XDR.
    pub xdr: String,
    /// The `LedgerEntryExt`, base64 XDR. Older RPCs omit it; it is then
    /// replayed as `V0`.
    #[serde(default)]
    pub ext_xdr: Option<String>,
    /// The ledger that last changed the entry; `None` when the RPC omits it,
    /// which is reported as unknown drift.
    #[serde(default)]
    pub last_modified_ledger_seq: Option<u32>,
    /// The last ledger the entry is live in; `None` for entries without a TTL
    /// (accounts, trustlines and the like).
    #[serde(default)]
    pub live_until_ledger_seq: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct GetLedgerEntriesResponse {
    #[serde(default)]
    entries: Vec<RawLedgerEntry>,
}

/// One ledger from `getLedgers`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawLedger {
    /// The ledger's sequence number.
    pub sequence: u32,
    /// The `LedgerHeaderHistoryEntry`, base64 XDR.
    pub header_xdr: String,
    /// The full `LedgerCloseMeta`, as a live RPC sends it.
    #[serde(default)]
    pub metadata_xdr: Option<String>,
    /// What a fixture keeps of `metadataXdr`: the changes to the entries it
    /// recorded. Not an RPC field; see `transport` and `history`.
    #[serde(default, rename = "sororewindChanges")]
    pub changes: Option<Vec<RawChangeGroup>>,
}

/// The ledger entry changes one transaction made in one phase of a ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawChangeGroup {
    /// When in the ledger these changes were applied.
    pub phase: crate::history::Phase,
    /// Zero-based apply position of the transaction in its ledger.
    pub tx: u32,
    /// `LedgerEntryChanges`, base64 XDR.
    pub changes_xdr: String,
}

#[derive(Debug, Deserialize)]
struct GetLedgersResponse {
    #[serde(default)]
    ledgers: Vec<RawLedger>,
}

/// One page of `getTransactions`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionsPage {
    /// The transactions on this page, oldest first.
    #[serde(default)]
    pub transactions: Vec<RawTransaction>,
    /// Where the next page starts; pass it back to `get_transactions`. `None`
    /// when the RPC sends none.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `getNetwork`: the identity of the network the RPC serves.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInfo {
    /// The network passphrase, e.g. `Test SDF Network ; September 2015`.
    pub passphrase: String,
}

/// `getHealth`. Every field is optional on the wire: a field the RPC leaves
/// out is `None`, never a made-up zero.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    /// The newest ledger the RPC has ingested.
    #[serde(default)]
    pub latest_ledger: Option<u32>,
    /// The oldest ledger the RPC still keeps.
    #[serde(default)]
    pub oldest_ledger: Option<u32>,
    /// How many ledgers the RPC keeps, counting back from the latest.
    #[serde(default)]
    pub ledger_retention_window: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RpcEnvelope<T> {
    result: Option<T>,
    error: Option<RpcErrorBody>,
}

#[derive(Debug, Deserialize)]
struct RpcErrorBody {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    data: Option<Value>,
}

/// The typed Soroban RPC client; every call goes through its [`Transport`].
pub struct RpcClient {
    transport: Box<dyn Transport>,
}

impl RpcClient {
    /// A client that sends every call through `transport`.
    pub fn new(transport: impl Transport + 'static) -> Self {
        Self {
            transport: Box::new(transport),
        }
    }

    /// Where responses come from: the endpoint URL, or `fixture:<dir>`.
    pub fn origin(&self) -> String {
        self.transport.origin()
    }

    fn error(&self, method: &str, code: Option<i64>, detail: String) -> RewindError {
        RewindError::Rpc {
            origin: self.origin(),
            method: method.to_string(),
            code,
            detail,
        }
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let text = self.transport.request(method, &params).await?;

        let parsed: RpcEnvelope<T> = serde_json::from_str(&text).map_err(|e| {
            self.error(
                method,
                None,
                format!(
                    "unparseable response: {e}; body was {}",
                    truncate(&text, 300)
                ),
            )
        })?;

        if let Some(err) = parsed.error {
            let detail = match (err.message, err.data) {
                (Some(m), Some(d)) => format!("{m} ({d})"),
                (Some(m), None) => m,
                (None, Some(d)) => d.to_string(),
                (None, None) => "an error with no message".to_string(),
            };
            return Err(self.error(method, err.code, detail));
        }

        parsed.result.ok_or_else(|| {
            self.error(
                method,
                None,
                "response had neither `result` nor `error`".to_string(),
            )
        })
    }

    /// `getHealth`: which ledgers the RPC holds.
    pub async fn health(&self) -> Result<Health> {
        self.call("getHealth", json!({})).await
    }

    /// `getNetwork`: the passphrase of the network the RPC serves.
    pub async fn network(&self) -> Result<NetworkInfo> {
        self.call("getNetwork", json!({})).await
    }

    /// `getTransaction` for `hash`. A hash the RPC does not know comes back as
    /// [`TxStatus::NotFound`], not as an error.
    pub async fn get_transaction(&self, hash: &TxHash) -> Result<RawTransaction> {
        self.call("getTransaction", json!({ "hash": hash.as_str() }))
            .await
    }

    /// Fetch exactly one ledger; `None` when the RPC no longer has it.
    pub async fn get_ledger(&self, sequence: u32) -> Result<Option<RawLedger>> {
        Ok(self
            .get_ledgers(sequence, 1)
            .await?
            .into_iter()
            .find(|ledger| ledger.sequence == sequence))
    }

    /// Fetch up to `limit` consecutive ledgers starting at `start`, metadata
    /// included. The RPC returns fewer when it has not closed them yet, and
    /// none when `start` is outside the ledgers it keeps.
    pub async fn get_ledgers(&self, start: u32, limit: u32) -> Result<Vec<RawLedger>> {
        // The limit lives under `pagination`; at the top level it is ignored
        // and the RPC falls back to 50 ledgers (~14 MB of metadata).
        let response: Result<GetLedgersResponse> = self
            .call(
                "getLedgers",
                json!({ "startLedger": start, "pagination": { "limit": limit } }),
            )
            .await;
        match response {
            Ok(response) => Ok(response.ledgers),
            // "No such ledger" is an answer, not a failure of the RPC.
            Err(err) if err.is_ledger_out_of_range() => {
                log::debug!(target: "rpc", "{err}");
                Ok(Vec::new())
            }
            Err(err) => Err(err),
        }
    }

    /// One page of transactions, oldest first. Start with `start_ledger`, then
    /// pass the previous page's cursor.
    pub async fn get_transactions(
        &self,
        start_ledger: u32,
        cursor: Option<&str>,
    ) -> Result<TransactionsPage> {
        let params = match cursor {
            Some(cursor) => json!({
                "pagination": { "cursor": cursor, "limit": MAX_TRANSACTIONS_PER_PAGE }
            }),
            None => json!({
                "startLedger": start_ledger,
                "pagination": { "limit": MAX_TRANSACTIONS_PER_PAGE }
            }),
        };
        self.call("getTransactions", params).await
    }

    /// Fetch ledger entries by their base64-encoded `LedgerKey` XDR.
    ///
    /// Returns one item per requested key, in request order, `None` where the
    /// RPC has no entry. The RPC caps the keys per call and answers in an
    /// unspecified order, so this chunks, fetches the chunks concurrently and
    /// puts everything back where the caller asked for it.
    pub async fn get_ledger_entries(&self, keys: &[String]) -> Result<Vec<Option<RawLedgerEntry>>> {
        let chunks = keys.chunks(MAX_KEYS_PER_CALL).map(|chunk| async move {
            let response: GetLedgerEntriesResponse = self
                .call(
                    "getLedgerEntries",
                    json!({ "keys": chunk, "xdrFormat": "base64" }),
                )
                .await?;
            Ok::<_, RewindError>(response.entries)
        });
        // A few at a time: a footprint is rarely more than one chunk, and a
        // public RPC answers a burst with 429s.
        let returned: Vec<Vec<RawLedgerEntry>> = futures_util::stream::iter(chunks)
            .buffered(MAX_CONCURRENT_CALLS)
            .try_collect()
            .await?;
        Ok(order_entries(
            keys,
            returned.into_iter().flatten().collect(),
        ))
    }
}

/// Line the returned entries up with the keys that were asked for.
fn order_entries(
    requested: &[String],
    returned: Vec<RawLedgerEntry>,
) -> Vec<Option<RawLedgerEntry>> {
    let positions: HashMap<&str, usize> = requested
        .iter()
        .enumerate()
        .rev() // so the first occurrence of a repeated key wins
        .map(|(i, key)| (key.as_str(), i))
        .collect();

    let mut ordered: Vec<Option<RawLedgerEntry>> = requested.iter().map(|_| None).collect();
    for entry in returned {
        if let Some(&i) = positions.get(entry.key.as_str()) {
            ordered[i] = Some(entry);
        }
    }
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemoryTransport;

    fn entry(key: &str) -> RawLedgerEntry {
        RawLedgerEntry {
            key: key.to_string(),
            xdr: format!("body-of-{key}"),
            ext_xdr: None,
            last_modified_ledger_seq: None,
            live_until_ledger_seq: None,
        }
    }

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn orders_entries_by_request() {
        let ordered = order_entries(
            &keys(&["a", "b", "c"]),
            vec![entry("c"), entry("a"), entry("unrequested")],
        );
        let got: Vec<Option<&str>> = ordered
            .iter()
            .map(|e| e.as_ref().map(|e| e.key.as_str()))
            .collect();
        assert_eq!(got, [Some("a"), None, Some("c")]);
        assert!(order_entries(&[], vec![entry("x")]).is_empty());
    }

    #[tokio::test]
    async fn typed_calls_go_through_the_transport() {
        let transport = MemoryTransport::new()
            .on("getNetwork", &json!({"result": {"passphrase": "Test SDF Network ; September 2015"}}))
            .on("getTransaction", &json!({"result": {"status": "NOT_FOUND"}}))
            .on("getLedgers", &json!({"result": {"ledgers": [
                {"sequence": 7, "ledgerCloseTime": "100", "headerXdr": "AAAA", "metadataXdr": "kept"}
            ]}}));
        let calls = transport.calls();
        let rpc = RpcClient::new(transport);

        assert_eq!(
            rpc.network().await.unwrap().passphrase,
            "Test SDF Network ; September 2015"
        );
        let hash: TxHash = "cd".repeat(32).parse().unwrap();
        assert_eq!(
            rpc.get_transaction(&hash).await.unwrap().status,
            TxStatus::NotFound
        );
        assert_eq!(
            rpc.get_ledger(7)
                .await
                .unwrap()
                .unwrap()
                .metadata_xdr
                .as_deref(),
            Some("kept")
        );
        assert!(rpc.get_ledger(8).await.unwrap().is_none());

        let calls = calls.lock().unwrap();
        assert_eq!(calls[1].1, json!({"hash": hash.as_str()}));
        assert_eq!(
            calls[2].1,
            json!({"startLedger": 7, "pagination": {"limit": 1}}),
            "exactly one ledger must be requested"
        );
    }

    #[tokio::test]
    async fn ledger_entries_are_chunked_and_reassembled() {
        let wanted: Vec<String> = (0..MAX_KEYS_PER_CALL + 5)
            .map(|i| format!("k{i}"))
            .collect();
        let transport = MemoryTransport::new().on_fn("getLedgerEntries", |params| {
            // Answer each chunk in reverse, dropping one key, like a real RPC might.
            let entries: Vec<Value> = params["keys"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .filter(|k| k.as_str() != Some("k3"))
                .map(|k| json!({"key": k, "xdr": "AAAA"}))
                .collect();
            json!({"result": {"entries": entries}})
        });
        let calls = transport.calls();
        let rpc = RpcClient::new(transport);

        let entries = rpc.get_ledger_entries(&wanted).await.unwrap();
        assert_eq!(entries.len(), wanted.len());
        for (i, (key, entry)) in wanted.iter().zip(&entries).enumerate() {
            match entry {
                Some(entry) => assert_eq!(&entry.key, key),
                None => assert_eq!(i, 3),
            }
        }
        let sizes: Vec<usize> = calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, params)| params["keys"].as_array().unwrap().len())
            .collect();
        assert_eq!(sizes, [MAX_KEYS_PER_CALL, 5]);
    }

    #[tokio::test]
    async fn rpc_errors_are_worded_for_people() {
        let rpc = RpcClient::new(
            MemoryTransport::new()
                .on("getHealth", &json!({"error": {"code": -32601, "message": "method not found", "data": "getHealth"}}))
                .on("getNetwork", &json!({"jsonrpc": "2.0"}))
                .on_text("getTransaction", "<html>502</html>"),
        );
        let err = rpc.health().await.unwrap_err().to_string();
        assert_eq!(
            err,
            "the RPC (memory) returned an error for getHealth: method not found (\"getHealth\") \
             (JSON-RPC code -32601)"
        );
        let err = rpc.network().await.unwrap_err().to_string();
        assert!(err.contains("neither `result` nor `error`"), "{err}");
        let hash: TxHash = "cd".repeat(32).parse().unwrap();
        let err = rpc.get_transaction(&hash).await.unwrap_err().to_string();
        assert!(
            err.contains("unparseable response") && err.contains("<html>502</html>"),
            "{err}"
        );
    }
}
