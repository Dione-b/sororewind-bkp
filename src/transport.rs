//! Where JSON-RPC responses come from.
//!
//! [`Transport`] is the seam between the typed client in `rpc.rs` and the
//! outside world. Three implementations live here:
//!
//! - [`HttpTransport`] talks to a live RPC;
//! - [`FixtureTransport`] answers from a recorded directory, offline;
//! - [`Recording`] wraps any transport and writes what passes through it.
//!
//! # Fixture layout (version 2)
//!
//! ```text
//! fixture.json                  { "version": 2, "hash": "<tx hash>" }
//! ledgerEntries.json            every ledger entry seen, keyed by its LedgerKey
//! <method>.json                 response to a call with empty params
//! <method>-<params digest>.json response to a call with params
//! ```
//!
//! Ledger entries are stored per key rather than per request, so a fixture
//! answers `getLedgerEntries` however the client chooses to batch its keys.
//! Other responses are addressed by a digest of their canonical params, so two
//! calls to one method never collide. `getLedgers` responses are stored
//! without `metadataXdr`, which is 100 KB to a few MB per ledger. Where a
//! replay reads it (to rebuild drifted entries, see `history`), the recording
//! keeps the part it needs in its place as `sororewindChanges`: the ledger
//! entry changes that touch the keys in `ledgerEntries.json` or their TTLs.

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{Result, RewindError};
use crate::hash::TxHash;

/// How long one RPC call may take before it is reported as unreachable.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Version of the on-disk fixture layout described in the module docs.
pub const FIXTURE_VERSION: u32 = 2;

const MANIFEST_FILE: &str = "fixture.json";
const ENTRIES_FILE: &str = "ledgerEntries.json";

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A source of raw JSON-RPC response bodies.
pub trait Transport: Send + Sync {
    /// Perform one call and return the response body, undecoded.
    fn request<'a>(&'a self, method: &'a str, params: &'a Value) -> BoxFuture<'a, Result<String>>;

    /// Where the responses come from, for the report and for error messages.
    fn origin(&self) -> String;
}

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn request<'a>(&'a self, method: &'a str, params: &'a Value) -> BoxFuture<'a, Result<String>> {
        (**self).request(method, params)
    }

    fn origin(&self) -> String {
        (**self).origin()
    }
}

// --- live RPC ---------------------------------------------------------------

pub struct HttpTransport {
    url: String,
    http: reqwest::Client,
}

impl HttpTransport {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("sororewind/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(RewindError::HttpClient)?;
        Ok(Self {
            url: url.into(),
            http,
        })
    }

    fn unreachable(&self, source: reqwest::Error) -> RewindError {
        let detail = if source.is_timeout() {
            format!("timed out after {}s", REQUEST_TIMEOUT.as_secs())
        } else if source.is_connect() {
            format!("connection failed: {source}")
        } else {
            source.to_string()
        };
        RewindError::RpcUnreachable {
            url: self.url.clone(),
            detail,
            source,
        }
    }
}

impl Transport for HttpTransport {
    fn request<'a>(&'a self, method: &'a str, params: &'a Value) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params,
            });
            log::trace!(target: "rpc", "→ {method} {params}");

            let response = self
                .http
                .post(&self.url)
                .json(&body)
                .send()
                .await
                .map_err(|e| self.unreachable(e))?;
            let status = response.status();
            let text = response.text().await.map_err(|e| self.unreachable(e))?;

            if !status.is_success() {
                return Err(RewindError::Rpc {
                    origin: self.origin(),
                    method: method.to_string(),
                    detail: format!("HTTP {status}: {}", truncate(&text, 300)),
                });
            }
            log::trace!(target: "rpc", "← {method} {}", truncate(&text, 400));
            Ok(text)
        })
    }

    fn origin(&self) -> String {
        self.url.clone()
    }
}

// --- fixtures ---------------------------------------------------------------

/// `fixture.json`: identifies a recording and the layout it was written in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureManifest {
    pub version: u32,
    /// The transaction this fixture was recorded for.
    pub hash: String,
}

impl FixtureManifest {
    pub async fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(MANIFEST_FILE);
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(RewindError::fixture_path(&path))?;
        let manifest: FixtureManifest =
            serde_json::from_str(&text).map_err(|e| RewindError::FixtureInvalid {
                path: path.clone(),
                reason: e.to_string(),
            })?;
        if manifest.version != FIXTURE_VERSION {
            return Err(RewindError::FixtureInvalid {
                path,
                reason: format!(
                    "it uses fixture layout version {}, this build reads version \
                     {FIXTURE_VERSION}; re-record it",
                    manifest.version
                ),
            });
        }
        Ok(manifest)
    }

    pub fn tx_hash(&self) -> Result<TxHash> {
        self.hash.parse()
    }
}

/// `ledgerEntries.json`: every ledger entry a recording saw. `null` marks a
/// key the RPC was asked for and did not have, which is different from a key
/// that was never requested.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntryStore {
    #[serde(default)]
    latest_ledger: u64,
    #[serde(default)]
    entries: BTreeMap<String, Option<Value>>,
}

pub struct FixtureTransport {
    dir: PathBuf,
    entries: EntryStore,
}

impl FixtureTransport {
    pub async fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        FixtureManifest::load(&dir).await?;

        let path = dir.join(ENTRIES_FILE);
        let entries = match tokio::fs::read_to_string(&path).await {
            Ok(text) => serde_json::from_str(&text).map_err(|e| RewindError::FixtureInvalid {
                path: path.clone(),
                reason: e.to_string(),
            })?,
            // A recording that never reached `getLedgerEntries` has no store.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => EntryStore::default(),
            Err(e) => return Err(RewindError::fixture_path(&path)(e)),
        };
        Ok(Self { dir, entries })
    }

    fn ledger_entries(&self, params: &Value) -> Result<String> {
        let mut found = Vec::new();
        for key in requested_keys(params) {
            match self.entries.entries.get(key) {
                Some(Some(entry)) => found.push(entry),
                Some(None) => {}
                None => {
                    return Err(RewindError::FixtureMissing {
                        dir: self.dir.clone(),
                        what: format!("ledger key {key}"),
                    })
                }
            }
        }
        Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "entries": found, "latestLedger": self.entries.latest_ledger },
        })
        .to_string())
    }
}

impl Transport for FixtureTransport {
    fn request<'a>(&'a self, method: &'a str, params: &'a Value) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            if method == "getLedgerEntries" {
                log::trace!(target: "fixture", "← {method} from {ENTRIES_FILE}");
                return self.ledger_entries(params);
            }
            let path = self.dir.join(response_file(method, params));
            log::trace!(target: "fixture", "← {method} {}", path.display());
            tokio::fs::read_to_string(&path)
                .await
                .map_err(RewindError::fixture_path(&path))
        })
    }

    fn origin(&self) -> String {
        format!("fixture:{}", self.dir.display())
    }
}

// --- recording --------------------------------------------------------------

/// Writes every response of the wrapped transport into a fixture directory.
///
/// A recording that cannot be written is a failed run: a silently incomplete
/// fixture is worse than no fixture.
pub struct Recording<T> {
    inner: T,
    dir: PathBuf,
    entries: tokio::sync::Mutex<EntryStore>,
}

impl<T: Transport> Recording<T> {
    pub async fn create(inner: T, dir: impl Into<PathBuf>, hash: &TxHash) -> Result<Self> {
        let dir = dir.into();
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(RewindError::record_path(&dir))?;
        let manifest = FixtureManifest {
            version: FIXTURE_VERSION,
            hash: hash.to_string(),
        };
        write_json(&dir.join(MANIFEST_FILE), &manifest).await?;
        Ok(Self {
            inner,
            dir,
            entries: tokio::sync::Mutex::new(EntryStore::default()),
        })
    }

    async fn record_entries(&self, params: &Value, text: &str) -> Result<()> {
        let Ok(response) = serde_json::from_str::<Value>(text) else {
            return Ok(());
        };
        // An error response is reported by the client; there is nothing to keep.
        let Some(result) = response.get("result") else {
            return Ok(());
        };
        let returned: BTreeMap<&str, &Value> = result["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| Some((e.get("key")?.as_str()?, e)))
            .collect();

        // Concurrent chunks merge into one store; the lock also serialises
        // the writes so the file always holds a complete document.
        let mut store = self.entries.lock().await;
        for key in requested_keys(params) {
            store
                .entries
                .insert(key.to_string(), returned.get(key).map(|e| (*e).clone()));
        }
        if let Some(latest) = result["latestLedger"].as_u64() {
            store.latest_ledger = store.latest_ledger.max(latest);
        }
        write_json(&self.dir.join(ENTRIES_FILE), &*store).await
    }
}

impl<T> Recording<T> {
    /// The ledger keys recorded so far and the keys of their TTL entries,
    /// XDR-encoded: the entries whose history is worth keeping.
    async fn recorded_keys(&self) -> HashSet<Vec<u8>> {
        let store = self.entries.lock().await;
        let mut keys = HashSet::new();
        for key in store.entries.keys() {
            let Ok(key) = crate::codec::b64_decode(key, "a ledger key") else {
                continue;
            };
            keys.extend(crate::history::ttl_key(&key));
            keys.insert(key);
        }
        keys
    }
}

impl<T: Transport> Transport for Recording<T> {
    fn request<'a>(&'a self, method: &'a str, params: &'a Value) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let text = self.inner.request(method, params).await?;
            if method == "getLedgerEntries" {
                self.record_entries(params, &text).await?;
            } else {
                let path = self.dir.join(response_file(method, params));
                log::debug!(target: "record", "{method} → {}", path.display());
                let keep = self.recorded_keys().await;
                tokio::fs::write(&path, slim(method, &text, &keep))
                    .await
                    .map_err(RewindError::record_path(&path))?;
            }
            Ok(text)
        })
    }

    fn origin(&self) -> String {
        self.inner.origin()
    }
}

async fn write_json<V: Serialize>(path: &Path, value: &V) -> Result<()> {
    // Pretty-printed: fixtures are committed and diffed.
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|e| RewindError::record_path(path)(std::io::Error::other(e)))?;
    text.push('\n');
    tokio::fs::write(path, text)
        .await
        .map_err(RewindError::record_path(path))
}

/// Drop response fields sororewind never reads and that dominate fixture
/// size. Ledger metadata is reduced to the changes that touch `keep`; with no
/// keys recorded yet (the header is read first) it is dropped altogether.
fn slim(method: &str, text: &str, keep: &HashSet<Vec<u8>>) -> String {
    if method != "getLedgers" {
        return text.to_string();
    }
    let Ok(mut response) = serde_json::from_str::<Value>(text) else {
        return text.to_string();
    };
    if let Some(ledgers) = response
        .pointer_mut("/result/ledgers")
        .and_then(Value::as_array_mut)
    {
        for ledger in ledgers.iter_mut().filter_map(Value::as_object_mut) {
            let metadata = ledger.remove("metadataXdr");
            ledger.remove("metadataJson");
            if keep.is_empty() {
                continue;
            }
            let changes = metadata
                .as_ref()
                .and_then(Value::as_str)
                .and_then(|metadata| crate::history::slim(metadata, keep))
                .and_then(|changes| serde_json::to_value(changes).ok());
            if let Some(changes) = changes {
                ledger.insert("sororewindChanges".to_string(), changes);
            }
        }
    }
    response.to_string()
}

fn requested_keys(params: &Value) -> impl Iterator<Item = &str> {
    params["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

/// File a response is stored under. Derived from the full request, so it is
/// stable across runs and unique per distinct call.
fn response_file(method: &str, params: &Value) -> String {
    let empty = match params {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        _ => false,
    };
    if empty {
        return format!("{method}.json");
    }
    let mut canon = String::new();
    canonical(params, &mut canon);
    let digest = Sha256::digest(canon.as_bytes());
    format!("{method}-{}.json", hex::encode(&digest[..8]))
}

/// JSON with object keys sorted and no insignificant whitespace, so equal
/// params always produce equal bytes.
fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// Cut `text` to at most `max` bytes without splitting a character.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemoryTransport;
    use serde_json::json;

    fn hash() -> TxHash {
        "ab".repeat(32).parse().unwrap()
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdef", 3), "abc…");
        // 'é' is two bytes; cutting in the middle backs off to the boundary.
        assert_eq!(truncate("aé", 2), "a…");
        assert_eq!(truncate("…", 1), "…");
    }

    #[test]
    fn response_files_are_canonical_and_distinct() {
        assert_eq!(response_file("getNetwork", &json!({})), "getNetwork.json");
        assert_eq!(response_file("getHealth", &Value::Null), "getHealth.json");

        let a = response_file(
            "getLedgers",
            &json!({"startLedger": 5, "pagination": {"limit": 1}}),
        );
        let b = response_file(
            "getLedgers",
            &json!({"pagination": {"limit": 1}, "startLedger": 5}),
        );
        let c = response_file(
            "getLedgers",
            &json!({"startLedger": 6, "pagination": {"limit": 1}}),
        );
        assert_eq!(a, b, "key order must not matter");
        assert_ne!(a, c, "different params must not collide");
        assert!(a.starts_with("getLedgers-") && a.ends_with(".json"));
    }

    #[test]
    fn slim_drops_ledger_metadata_only() {
        let text = json!({"result": {"ledgers": [
            {"sequence": 1, "headerXdr": "AAAA", "metadataXdr": "big"}
        ]}})
        .to_string();
        let none = HashSet::new();
        let slimmed: Value = serde_json::from_str(&slim("getLedgers", &text, &none)).unwrap();
        assert_eq!(
            slimmed["result"]["ledgers"][0],
            json!({"sequence": 1, "headerXdr": "AAAA"})
        );
        assert_eq!(slim("getTransaction", &text, &none), text);
        assert_eq!(slim("getLedgers", "not json", &none), "not json");

        // With keys recorded, undecodable metadata is still dropped.
        let some = HashSet::from([vec![1u8]]);
        let slimmed: Value = serde_json::from_str(&slim("getLedgers", &text, &some)).unwrap();
        assert_eq!(
            slimmed["result"]["ledgers"][0],
            json!({"sequence": 1, "headerXdr": "AAAA"})
        );
    }

    #[tokio::test]
    async fn recording_replays_offline_regardless_of_chunking() {
        let dir = tempfile::tempdir().unwrap();
        let live = MemoryTransport::new()
            .on("getNetwork", &json!({"result": {"passphrase": "p"}}))
            .on(
                "getLedgerEntries",
                &json!({"result": {"latestLedger": 9, "entries": [
                    {"key": "k1", "xdr": "e1"},
                    {"key": "k3", "xdr": "e3"},
                ]}}),
            );
        let recording = Recording::create(live, dir.path(), &hash()).await.unwrap();

        recording.request("getNetwork", &json!({})).await.unwrap();
        // Recorded as two chunks...
        recording
            .request("getLedgerEntries", &json!({"keys": ["k1", "k2"]}))
            .await
            .unwrap();
        recording
            .request("getLedgerEntries", &json!({"keys": ["k3"]}))
            .await
            .unwrap();

        let manifest = FixtureManifest::load(dir.path()).await.unwrap();
        assert_eq!(manifest.tx_hash().unwrap(), hash());

        // ...and replayed as one.
        let fixture = FixtureTransport::open(dir.path()).await.unwrap();
        let text = fixture
            .request("getLedgerEntries", &json!({"keys": ["k3", "k2", "k1"]}))
            .await
            .unwrap();
        let response: Value = serde_json::from_str(&text).unwrap();
        let keys: Vec<&str> = response["result"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["k3", "k1"], "k2 was recorded as absent");
        assert_eq!(response["result"]["latestLedger"], 9);

        let network = fixture.request("getNetwork", &json!({})).await.unwrap();
        assert!(network.contains("passphrase"));
        assert!(fixture.origin().starts_with("fixture:"));
    }

    #[tokio::test]
    async fn fixture_reports_what_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let err = FixtureTransport::open(dir.path()).await.err().unwrap();
        assert!(matches!(err, RewindError::FixtureIo { .. }), "{err}");

        tokio::fs::write(
            dir.path().join(MANIFEST_FILE),
            r#"{"version":1,"hash":"x"}"#,
        )
        .await
        .unwrap();
        let err = FixtureTransport::open(dir.path()).await.err().unwrap();
        assert!(
            err.to_string().contains("fixture layout version 1"),
            "{err}"
        );

        Recording::create(MemoryTransport::new(), dir.path(), &hash())
            .await
            .unwrap();
        let fixture = FixtureTransport::open(dir.path()).await.unwrap();
        let err = fixture
            .request("getLedgerEntries", &json!({"keys": ["never-recorded"]}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("ledger key never-recorded"),
            "{err}"
        );
        let err = fixture.request("getHealth", &json!({})).await.unwrap_err();
        assert!(matches!(err, RewindError::FixtureIo { .. }), "{err}");
    }

    #[tokio::test]
    async fn recording_failures_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let live = MemoryTransport::new().on("getNetwork", &json!({"result": {}}));
        let recording = Recording::create(live, dir.path().join("rec"), &hash())
            .await
            .unwrap();
        // Pull the directory out from under the recorder.
        tokio::fs::remove_dir_all(dir.path().join("rec"))
            .await
            .unwrap();
        let err = recording
            .request("getNetwork", &json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, RewindError::RecordIo { .. }), "{err}");
    }
}
