//! fetch: pull the raw materials for a replay out of a Soroban RPC.
//!
//! Everything here is deliberately dumb: no decoding, no interpretation. The
//! stage's only job is to turn JSON into typed records that `decode` can work
//! on, and to turn RPC-level failures into messages a human can act on.
//!
//! Two extra jobs, both about reproducibility: every response can be written to
//! a directory with `--record`, and later read back with `--fixture` so the
//! exact same replay runs offline. Recorded fixtures expire with the RPC's own
//! retention window, which is why the recording script exists.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::cli::Network;
use crate::error::{Result, RewindError};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// The subset of `getTransaction` we care about.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawTransaction {
    pub status: String,
    #[serde(default)]
    pub ledger: Option<u32>,
    #[serde(default)]
    pub envelope_xdr: Option<String>,
    #[serde(default)]
    pub result_xdr: Option<String>,
    #[serde(default)]
    pub result_meta_xdr: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTransactionResponse {
    #[serde(flatten)]
    pub tx: RawTransaction,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawLedgerEntry {
    pub key: String,
    pub xdr: String,
    #[serde(default)]
    pub ext_xdr: Option<String>,
    #[serde(default)]
    pub last_modified_ledger_seq: Option<u32>,
    #[serde(default)]
    pub live_until_ledger_seq: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetLedgerEntriesResponse {
    #[serde(default)]
    pub entries: Vec<RawLedgerEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawLedger {
    pub sequence: u32,
    pub ledger_close_time: String,
    pub header_xdr: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetLedgersResponse {
    #[serde(default)]
    pub ledgers: Vec<RawLedger>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInfo {
    pub passphrase: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    #[serde(default)]
    pub oldest_ledger: u32,
    #[serde(default)]
    pub ledger_retention_window: u32,
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
    data: Option<serde_json::Value>,
}

/// Where responses come from: the live network, or a recorded directory.
#[derive(Debug)]
enum Source {
    Http { url: String, http: reqwest::Client },
    Fixture { dir: PathBuf },
}

pub struct RpcClient {
    network: Network,
    verbose: u8,
    source: Source,
    /// When set, every live response is also written here.
    record: Option<PathBuf>,
}

impl RpcClient {
    pub fn new(
        network: Network,
        url: Option<String>,
        verbose: u8,
        fixture: Option<PathBuf>,
        record: Option<PathBuf>,
    ) -> Self {
        let source = match fixture {
            Some(dir) => Source::Fixture { dir },
            None => Source::Http {
                url: url.unwrap_or_else(|| network.default_rpc_url().to_string()),
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .user_agent(concat!("sororewind/", env!("CARGO_PKG_VERSION")))
                    .build()
                    .expect("reqwest client builder cannot fail with these settings"),
            },
        };
        Self {
            network,
            verbose,
            source,
            record,
        }
    }

    pub fn url(&self) -> String {
        match &self.source {
            Source::Http { url, .. } => url.clone(),
            Source::Fixture { dir } => format!("fixture:{}", dir.display()),
        }
    }

    /// File name a response is stored under. Stable across runs so a recorded
    /// directory can be replayed byte-for-byte.
    fn cache_file(dir: &Path, method: &str, key: &str) -> PathBuf {
        dir.join(format!("{method}-{key}.json"))
    }

    fn cache_key(method: &str, params: &serde_json::Value) -> String {
        match method {
            "getTransaction" => params["hash"].as_str().unwrap_or("unknown").to_string(),
            "getLedgers" => params["startLedger"].to_string(),
            "getNetwork" => "network".to_string(),
            "getHealth" => "health".to_string(),
            "getLedgerEntries" => {
                let joined = params["keys"]
                    .as_array()
                    .map(|keys| {
                        keys.iter()
                            .filter_map(|k| k.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                hex::encode(&Sha256::digest(joined.as_bytes())[..8])
            }
            other => hex::encode(&Sha256::digest(other.as_bytes())[..8]),
        }
    }

    async fn call<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T> {
        let key = Self::cache_key(method, &params);

        let text = match &self.source {
            Source::Fixture { dir } => {
                let path = Self::cache_file(dir, method, &key);
                if self.verbose > 1 {
                    eprintln!("[fixture] ← {method} {}", path.display());
                }
                std::fs::read_to_string(&path).map_err(|e| {
                    RewindError::State(format!(
                    "the fixture at {} could not be read ({e}); it was recorded for a different \
                     transaction or the directory is incomplete",
                    path.display()
                ))
                })?
            }
            Source::Http { url, http } => {
                let body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": method,
                    "params": params,
                });
                if self.verbose > 1 {
                    eprintln!("[rpc] → {method} {}", body["params"]);
                }

                let response = http.post(url).json(&body).send().await.map_err(|e| {
                    RewindError::RpcUnreachable {
                        network: self.network.name().to_string(),
                        url: url.clone(),
                        detail: describe_reqwest(&e),
                    }
                })?;

                let status = response.status();
                let text = response
                    .text()
                    .await
                    .map_err(|e| RewindError::RpcUnreachable {
                        network: self.network.name().to_string(),
                        url: url.clone(),
                        detail: format!("reading response body: {e}"),
                    })?;

                if !status.is_success() {
                    return Err(RewindError::Rpc {
                        network: self.network.name().to_string(),
                        method: method.to_string(),
                        detail: format!("HTTP {status}: {}", truncate(&text, 300)),
                    });
                }

                if self.verbose > 1 {
                    eprintln!("[rpc] ← {method} {}", truncate(&text, 400));
                }

                if let Some(dir) = &self.record {
                    let path = Self::cache_file(dir, method, &key);
                    if let Err(e) = write_response(&path, &text) {
                        if self.verbose > 0 {
                            eprintln!("[record] could not write {}: {e}", path.display());
                        }
                    }
                }

                text
            }
        };

        let parsed: RpcEnvelope<T> = serde_json::from_str(&text).map_err(|e| RewindError::Rpc {
            network: self.network.name().to_string(),
            method: method.to_string(),
            detail: format!(
                "unparseable response: {e}; body was {}",
                truncate(&text, 300)
            ),
        })?;

        if let Some(err) = parsed.error {
            let detail = match (err.message, err.data) {
                (Some(m), Some(d)) => format!("{m} ({d})"),
                (Some(m), None) => m,
                (None, Some(d)) => d.to_string(),
                (None, None) => format!("error code {:?}", err.code),
            };
            return Err(RewindError::Rpc {
                network: self.network.name().to_string(),
                method: method.to_string(),
                detail,
            });
        }

        parsed.result.ok_or_else(|| RewindError::Rpc {
            network: self.network.name().to_string(),
            method: method.to_string(),
            detail: "response had neither `result` nor `error`".to_string(),
        })
    }

    pub async fn health(&self) -> Result<Health> {
        self.call("getHealth", serde_json::json!({})).await
    }

    pub async fn network_info(&self) -> Result<NetworkInfo> {
        self.call("getNetwork", serde_json::json!({})).await
    }

    pub async fn get_transaction(&self, hash: &str) -> Result<GetTransactionResponse> {
        self.call("getTransaction", serde_json::json!({ "hash": hash }))
            .await
    }

    pub async fn get_ledgers(&self, start: u32, limit: u32) -> Result<GetLedgersResponse> {
        self.call(
            "getLedgers",
            serde_json::json!({ "startLedger": start, "limit": limit }),
        )
        .await
    }

    /// Fetch ledger entries by their base64-encoded `LedgerKey` XDR.
    ///
    /// The RPC caps the number of keys per call, so this pages automatically and
    /// preserves the caller's ordering (the replay needs footprint order).
    pub async fn get_ledger_entries(&self, keys_b64: &[String]) -> Result<Vec<RawLedgerEntry>> {
        const MAX_KEYS: usize = 50;

        let mut out = Vec::with_capacity(keys_b64.len());
        for chunk in keys_b64.chunks(MAX_KEYS) {
            let resp: GetLedgerEntriesResponse = self
                .call(
                    "getLedgerEntries",
                    serde_json::json!({ "keys": chunk, "xdrFormat": "base64" }),
                )
                .await?;
            out.extend(resp.entries);
        }
        Ok(out)
    }
}

fn write_response(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)
}

/// `getLedgerEntries` returns entries in an unspecified order relative to the
/// request, so re-order by looking each requested key up in the response.
pub fn order_entries(
    requested: &[String],
    returned: Vec<RawLedgerEntry>,
) -> Vec<Option<RawLedgerEntry>> {
    use std::collections::HashMap;
    let mut by_key: HashMap<String, RawLedgerEntry> = HashMap::new();
    for e in returned {
        by_key.insert(e.key.clone(), e);
    }
    requested.iter().map(|k| by_key.remove(k)).collect()
}

pub fn b64_decode(s: &str, what: &str) -> Result<Vec<u8>> {
    B64.decode(s.trim())
        .map_err(|e| RewindError::Decode(format!("invalid base64 in {what}: {e}")))
}

/// `getLedgerEntries` hands back an entry as separate pieces — the body in
/// `xdr` (a `LedgerEntryData`), the extension in `extXdr`, and the bookkeeping
/// fields alongside. Reassemble them into the `LedgerEntry` the host wants.
pub fn assemble_entry(raw: &RawLedgerEntry) -> Result<stellar_xdr::LedgerEntry> {
    use stellar_xdr::{LedgerEntry, LedgerEntryData, LedgerEntryExt, Limits, ReadXdr};

    let body = b64_decode(&raw.xdr, "ledger entry body")?;
    let data = LedgerEntryData::from_xdr(&body, Limits::none())
        .map_err(|e| RewindError::State(format!("decoding ledger entry body: {e}")))?;

    // Older recordings and simpler nodes may omit the field; `LedgerEntryExt`
    // default is `V0`, which is what an absent extension means.
    let ext = match raw.ext_xdr.as_deref() {
        Some(text) if !text.is_empty() => {
            let bytes = b64_decode(text, "ledger entry ext")?;
            LedgerEntryExt::from_xdr(&bytes, Limits::none())
                .map_err(|e| RewindError::State(format!("decoding ledger entry ext: {e}")))?
        }
        _ => LedgerEntryExt::V0,
    };

    Ok(LedgerEntry {
        last_modified_ledger_seq: raw.last_modified_ledger_seq.unwrap_or(0),
        data,
        ext,
    })
}

pub fn b64_encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

fn describe_reqwest(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out after 30s".to_string()
    } else if e.is_connect() {
        format!("connection failed: {e}")
    } else {
        e.to_string()
    }
}
