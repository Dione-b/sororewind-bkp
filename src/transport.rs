//! Where JSON-RPC responses come from.
//!
//! [`Transport`] is the seam between the typed client in `rpc.rs` and the
//! outside world. Three implementations live here:
//!
//! - [`HttpTransport`] talks to a live RPC, retrying transient failures;
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

/// The boxed future a [`Transport`] returns, so the trait stays object-safe.
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

/// How long connecting to the RPC may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How many times a call is retried after a transient failure.
pub const MAX_RETRIES: u32 = 3;

/// The first retry waits this long; each later one twice as long as the last.
const RETRY_BASE: Duration = Duration::from_millis(400);

/// The longest a `Retry-After` header is obeyed.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);

/// A [`Transport`] that sends each call to a live RPC endpoint over HTTP,
/// retrying transient failures.
pub struct HttpTransport {
    url: String,
    /// What is shown of `url`: see [`redact`].
    origin: String,
    http: reqwest::Client,
    retry_base: Duration,
}

/// An RPC URL with everything that can hold a credential removed.
///
/// Providers put API keys in the path (`/v1/<key>`), the query (`?key=`) or
/// the userinfo (`https://user:pass@host`). The URL ends up in the JSON
/// report and in error messages, which people paste into issues, so only the
/// scheme, host and port are shown; a path or query is replaced by `/…`.
pub fn redact(url: &str) -> String {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        // Not a URL: nothing in it can be told apart from a secret.
        return "<unparseable RPC URL>".to_string();
    };
    let host = parsed.host_str().unwrap_or("<no host>");
    let port = parsed
        .port()
        .map_or(String::new(), |port| format!(":{port}"));
    let rest = if matches!(parsed.path(), "" | "/") && parsed.query().is_none() {
        ""
    } else {
        "/…"
    };
    format!("{}://{host}{port}{rest}", parsed.scheme())
}

/// What became of one attempt at a call.
enum Attempt {
    Done(String),
    /// Worth another try: the reason, and how long the server asked us to
    /// wait, if it did.
    Transient(RewindError, Option<Duration>),
    Failed(RewindError),
}

impl HttpTransport {
    /// A transport for the RPC at `url`, with the default timeouts and retry
    /// policy. Fails only when the HTTP client cannot be built.
    pub fn new(url: impl Into<String>) -> Result<Self> {
        let url = url.into();
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("sororewind/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(RewindError::HttpClient)?;
        Ok(Self {
            origin: redact(&url),
            url,
            http,
            retry_base: RETRY_BASE,
        })
    }

    /// Wait this long before the first retry instead of the default.
    pub fn with_retry_base(mut self, base: Duration) -> Self {
        self.retry_base = base;
        self
    }

    fn unreachable(&self, source: reqwest::Error) -> RewindError {
        // reqwest's own message names the URL and stops there; the reason
        // (DNS, TLS, connection refused) is further down the source chain.
        let source = source.without_url();
        let mut detail = if source.is_timeout() {
            format!("timed out after {}s", REQUEST_TIMEOUT.as_secs())
        } else if source.is_connect() {
            "connection failed".to_string()
        } else {
            source.to_string()
        };
        let mut cause = std::error::Error::source(&source);
        while let Some(inner) = cause {
            let text = inner.to_string();
            if !detail.contains(&text) {
                detail.push_str(": ");
                detail.push_str(&text);
            }
            cause = inner.source();
        }
        RewindError::RpcUnreachable {
            url: self.origin.clone(),
            detail,
            source,
        }
    }

    async fn attempt(&self, method: &str, body: &Value) -> Attempt {
        let response = match self.http.post(&self.url).json(body).send().await {
            Ok(response) => response,
            Err(e) => {
                // Nothing was answered: a connect error or a timeout. Every
                // method sororewind calls is read-only, so trying again is safe.
                let transient = e.is_connect() || e.is_timeout();
                let error = self.unreachable(e);
                return if transient {
                    Attempt::Transient(error, None)
                } else {
                    Attempt::Failed(error)
                };
            }
        };
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|seconds| Duration::from_secs(seconds).min(MAX_RETRY_AFTER));
        let text = match response.text().await {
            Ok(text) => text,
            Err(e) => return Attempt::Transient(self.unreachable(e), None),
        };
        if status.is_success() {
            return Attempt::Done(text);
        }
        let error = RewindError::Rpc {
            origin: self.origin(),
            method: method.to_string(),
            code: None,
            detail: format!("HTTP {status}: {}", truncate(&text, 300)),
        };
        // A rate limit or an overloaded gateway is worth waiting out, even
        // when the provider phrases it as a JSON-RPC error.
        if matches!(status.as_u16(), 429 | 502 | 503 | 504) {
            return Attempt::Transient(error, retry_after);
        }
        // Some providers answer a JSON-RPC error with a non-2xx status; the
        // envelope says more than the status does, so let the client read it.
        let is_envelope = serde_json::from_str::<Value>(&text)
            .is_ok_and(|body| body.get("error").is_some_and(|error| !error.is_null()));
        if is_envelope {
            Attempt::Done(text)
        } else {
            Attempt::Failed(error)
        }
    }

    /// How long to wait before retry number `retry` (zero-based): the base
    /// doubled each time, plus up to half as much again so that concurrent
    /// calls do not all come back at once.
    fn backoff(&self, retry: u32) -> Duration {
        let wait = self.retry_base.saturating_mul(1 << retry.min(8));
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |now| u64::from(now.subsec_nanos()));
        let jitter = wait.as_millis() as u64 / 2;
        wait + Duration::from_millis(if jitter == 0 { 0 } else { nanos % jitter })
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

            let mut retry = 0;
            loop {
                match self.attempt(method, &body).await {
                    Attempt::Done(text) => {
                        log::trace!(target: "rpc", "← {method} {}", truncate(&text, 400));
                        return Ok(text);
                    }
                    Attempt::Transient(error, retry_after) if retry < MAX_RETRIES => {
                        let wait = retry_after.unwrap_or_else(|| self.backoff(retry));
                        retry += 1;
                        log::debug!(
                            target: "rpc",
                            "{method} failed ({error}); retry {retry} of {MAX_RETRIES} in {} ms",
                            wait.as_millis()
                        );
                        tokio::time::sleep(wait).await;
                    }
                    Attempt::Transient(error, _) | Attempt::Failed(error) => return Err(error),
                }
            }
        })
    }

    fn origin(&self) -> String {
        self.origin.clone()
    }
}

// --- fixtures ---------------------------------------------------------------

/// `fixture.json`: identifies a recording and the layout it was written in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureManifest {
    /// The fixture layout version; this build reads only [`FIXTURE_VERSION`].
    pub version: u32,
    /// The transaction this fixture was recorded for.
    pub hash: String,
}

impl FixtureManifest {
    /// Read and check `fixture.json` in `dir`. Fails when it is missing,
    /// unparseable or written in another layout version.
    pub async fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(MANIFEST_FILE);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            // The directory, not a file in it, is what is wrong.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(RewindError::FixtureInvalid {
                    path: dir.to_path_buf(),
                    reason: if dir.is_dir() {
                        format!("it has no {MANIFEST_FILE}; it is not a recorded fixture")
                    } else {
                        "there is no such directory".to_string()
                    },
                });
            }
            Err(e) => return Err(RewindError::fixture_path(&path)(e)),
        };
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

    /// The recorded hash, parsed and normalised.
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

/// A [`Transport`] that answers from a recorded fixture directory, offline.
pub struct FixtureTransport {
    dir: PathBuf,
    manifest: FixtureManifest,
    entries: EntryStore,
}

impl FixtureTransport {
    /// Open the fixture in `dir`. A missing `ledgerEntries.json` is an empty
    /// store, not an error.
    pub async fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        let manifest = FixtureManifest::load(&dir).await?;

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
        Ok(Self {
            dir,
            manifest,
            entries,
        })
    }

    /// The transaction this fixture was recorded for.
    pub fn tx_hash(&self) -> Result<TxHash> {
        self.manifest.tx_hash()
    }

    /// The hash to replay from this fixture: the recorded one, which
    /// `requested`, when given, has to be.
    pub fn hash_for(&self, requested: Option<&TxHash>) -> Result<TxHash> {
        let recorded = self.tx_hash()?;
        match requested {
            Some(requested) if *requested != recorded => Err(RewindError::FixtureHashMismatch {
                dir: self.dir.clone(),
                recorded,
                requested: requested.clone(),
            }),
            _ => Ok(recorded),
        }
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
                    });
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
            match tokio::fs::read_to_string(&path).await {
                Ok(text) => Ok(text),
                // Responses are filed by their request: no file, no such call
                // in the recording.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(RewindError::FixtureMissing {
                        dir: self.dir.clone(),
                        what: format!("{method} {params}"),
                    })
                }
                Err(e) => Err(RewindError::fixture_path(&path)(e)),
            }
        })
    }

    fn origin(&self) -> String {
        format!("fixture:{}", self.dir.display())
    }
}

// --- recording --------------------------------------------------------------

/// Writes every response of the wrapped transport into a fixture directory.
///
/// A recording is staged in a hidden sibling of its target (`.name.recording`
/// next to `name`) and only becomes `name` when [`RecordingHandle::commit`] is
/// called, in one rename. Until then an existing fixture at the target is
/// untouched, so a run that fails halfway, or is interrupted, can neither
/// destroy a working fixture nor leave one that mixes two transactions.
///
/// A recording that cannot be written is a failed run: a silently incomplete
/// fixture is worse than no fixture.
pub struct Recording<T> {
    inner: T,
    state: std::sync::Arc<RecordingState>,
}

struct RecordingState {
    target: PathBuf,
    staging: PathBuf,
    backup: PathBuf,
    entries: tokio::sync::Mutex<EntryStore>,
}

/// Decides what becomes of a [`Recording`] once the run it recorded is over.
#[derive(Clone)]
pub struct RecordingHandle(std::sync::Arc<RecordingState>);

impl<T: Transport> Recording<T> {
    /// Start recording `inner`'s responses for `hash` into `dir`, staged until
    /// [`RecordingHandle::commit`]. Whatever an interrupted run left in the
    /// staging directory is removed first.
    pub async fn create(inner: T, dir: impl Into<PathBuf>, hash: &TxHash) -> Result<Self> {
        let target: PathBuf = dir.into();
        let name = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| {
                RewindError::record_path(&target)(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "the directory has no name to record under",
                ))
            })?;
        let parent = match target.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        // Hidden, and named after the whole target name, so `a.v1` and `a.v2`
        // do not share a staging area and nothing mistakes it for a fixture.
        let staging = parent.join(format!(".{name}.recording"));
        let backup = parent.join(format!(".{name}.backup"));

        // A commit interrupted between its two renames leaves the old fixture
        // only in the backup: put it back before anything else touches it.
        // A backup that is not a whole fixture is just debris.
        if !tokio::fs::try_exists(&target).await.unwrap_or(true)
            && tokio::fs::try_exists(backup.join(MANIFEST_FILE))
                .await
                .unwrap_or(false)
        {
            tokio::fs::rename(&backup, &target)
                .await
                .map_err(RewindError::record_path(&backup))?;
        }
        refuse_non_fixture(&target).await?;

        // Whatever an interrupted run left behind is not worth keeping.
        remove_dir_if_present(&staging).await?;
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(RewindError::record_path(&staging))?;
        let manifest = FixtureManifest {
            version: FIXTURE_VERSION,
            hash: hash.to_string(),
        };
        write_json(&staging.join(MANIFEST_FILE), &manifest).await?;
        Ok(Self {
            inner,
            state: std::sync::Arc::new(RecordingState {
                target,
                staging,
                backup,
                entries: tokio::sync::Mutex::new(EntryStore::default()),
            }),
        })
    }

    /// The handle that commits or discards this recording.
    pub fn handle(&self) -> RecordingHandle {
        RecordingHandle(self.state.clone())
    }

    async fn record_entries(&self, params: &Value, text: &str) {
        let Ok(response) = serde_json::from_str::<Value>(text) else {
            return;
        };
        // An error response fails the run, and a failed run is not committed.
        let Some(result) = response.get("result") else {
            return;
        };
        let returned: BTreeMap<&str, &Value> = result["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| Some((e.get("key")?.as_str()?, e)))
            .collect();

        // Concurrent chunks merge into one store, written once on commit.
        let mut store = self.state.entries.lock().await;
        for key in requested_keys(params) {
            store
                .entries
                .insert(key.to_string(), returned.get(key).map(|e| (*e).clone()));
        }
        if let Some(latest) = result["latestLedger"].as_u64() {
            store.latest_ledger = store.latest_ledger.max(latest);
        }
    }

    /// The ledger keys recorded so far and the keys of their TTL entries,
    /// XDR-encoded: the entries whose history is worth keeping.
    async fn recorded_keys(&self) -> HashSet<Vec<u8>> {
        let store = self.state.entries.lock().await;
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

impl RecordingHandle {
    /// Where the recording ends up once committed.
    pub fn target(&self) -> &Path {
        &self.0.target
    }

    /// Make the recording the fixture at its target, replacing the one that
    /// was there. The old fixture is only removed once the new one is in
    /// place, and is put back if that fails.
    pub async fn commit(&self) -> Result<()> {
        let RecordingState {
            target,
            staging,
            backup,
            entries,
        } = &*self.0;
        {
            let store = entries.lock().await;
            // A recording that never reached `getLedgerEntries` has no store.
            if !store.entries.is_empty() {
                write_json(&staging.join(ENTRIES_FILE), &*store).await?;
            }
        }

        remove_dir_if_present(backup).await?;
        let had_old = match tokio::fs::rename(target, backup).await {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(RewindError::record_path(target)(e)),
        };
        if let Err(e) = tokio::fs::rename(staging, target).await {
            if had_old {
                let _ = tokio::fs::rename(backup, target).await;
            }
            let _ = tokio::fs::remove_dir_all(staging).await;
            return Err(RewindError::record_path(target)(e));
        }
        if had_old {
            remove_dir_if_present(backup).await?;
        }
        Ok(())
    }

    /// Throw the recording away; the fixture at the target stays as it was.
    pub async fn discard(&self) {
        let _ = tokio::fs::remove_dir_all(&self.0.staging).await;
    }
}

/// The commit replaces the target whole, so it has to be a fixture already,
/// an empty directory, or nothing: `--record ~/notes` must not delete notes.
async fn refuse_non_fixture(target: &Path) -> Result<()> {
    let refuse = |why: &str| {
        Err(RewindError::record_path(target)(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{why}; refusing to replace it with a recording"),
        )))
    };
    let meta = match tokio::fs::metadata(target).await {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(RewindError::record_path(target)(e)),
    };
    if !meta.is_dir() {
        return refuse("it exists and is not a directory");
    }
    if tokio::fs::try_exists(target.join(MANIFEST_FILE))
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }
    let mut entries = tokio::fs::read_dir(target)
        .await
        .map_err(RewindError::record_path(target))?;
    match entries.next_entry().await {
        Ok(None) => Ok(()),
        Ok(Some(_)) => refuse(&format!(
            "it is not empty and has no {MANIFEST_FILE}, so it is not a fixture"
        )),
        Err(e) => Err(RewindError::record_path(target)(e)),
    }
}

async fn remove_dir_if_present(dir: &Path) -> Result<()> {
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(RewindError::record_path(dir)(e)),
    }
}

impl<T: Transport> Transport for Recording<T> {
    fn request<'a>(&'a self, method: &'a str, params: &'a Value) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let text = self.inner.request(method, params).await?;
            if method == "getLedgerEntries" {
                self.record_entries(params, &text).await;
            } else {
                let path = self.state.staging.join(response_file(method, params));
                log::debug!(target: "record", "{method} → {}", path.display());
                let keep = self.recorded_keys().await;
                write_atomically(&path, slim(method, &text, &keep).as_bytes()).await?;
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
    write_atomically(path, text.as_bytes()).await
}

/// Write a file whole or not at all: to a temporary name, then renamed over
/// the destination, so an interrupted run never leaves truncated JSON.
async fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    tokio::fs::write(&temporary, bytes)
        .await
        .map_err(RewindError::record_path(path))?;
    tokio::fs::rename(&temporary, path)
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
    fn urls_are_redacted_before_they_are_shown() {
        // Nothing to hide: shown as is.
        assert_eq!(
            redact("https://soroban-testnet.stellar.org:443"),
            "https://soroban-testnet.stellar.org"
        );
        assert_eq!(redact("http://localhost:8000"), "http://localhost:8000");
        assert_eq!(redact("https://rpc.example/"), "https://rpc.example");
        // Keys in the path, the query or the userinfo are not.
        for url in [
            "https://rpc.provider.io/v1/SECRETKEY",
            "https://rpc.provider.io/?apikey=SECRETKEY",
            "https://user:SECRETKEY@rpc.provider.io/rpc",
        ] {
            let shown = redact(url);
            assert_eq!(shown, "https://rpc.provider.io/…", "{url}");
            assert!(!shown.contains("SECRETKEY"));
        }
        assert!(!redact("SECRETKEY not a url").contains("SECRETKEY"));
        // A path with no key in it is still hidden: it cannot be told apart.
        assert_eq!(
            redact("http://localhost:8000/rpc"),
            "http://localhost:8000/…"
        );
    }

    /// A one-connection-at-a-time HTTP server answering each request with
    /// the next canned `(status, extra headers, body)`.
    fn serve(
        responses: Vec<(u16, &'static str, &'static str)>,
    ) -> (String, std::thread::JoinHandle<usize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/SECRETKEY", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut served = 0;
            for (status, headers, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                // Read the request: headers, then `Content-Length` bytes.
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buffer).unwrap();
                    request.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n{headers}\r\n{body}",
                    body.len()
                )
                .unwrap();
                served += 1;
            }
            served
        });
        (url, handle)
    }

    fn fast(url: &str) -> HttpTransport {
        HttpTransport::new(url)
            .unwrap()
            .with_retry_base(Duration::from_millis(1))
    }

    #[tokio::test]
    async fn transient_failures_are_retried() {
        let ok = r#"{"jsonrpc":"2.0","id":1,"result":{"passphrase":"p"}}"#;
        let (url, server) = serve(vec![
            (503, "", "busy"),
            (429, "retry-after: 0\r\n", "slow down"),
            (200, "", ok),
        ]);
        let text = fast(&url).request("getNetwork", &json!({})).await.unwrap();
        assert_eq!(text, ok);
        assert_eq!(server.join().unwrap(), 3);
    }

    #[tokio::test]
    async fn retries_give_up_and_never_show_the_key() {
        let busy = (503, "", "busy");
        let (url, server) = serve(vec![busy; MAX_RETRIES as usize + 1]);
        let transport = fast(&url);
        let err = transport
            .request("getNetwork", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(server.join().unwrap(), MAX_RETRIES as usize + 1);
        let text = err.to_string();
        assert!(text.contains("HTTP 503") && text.contains("busy"), "{text}");
        assert!(!text.contains("SECRETKEY"), "{text}");
        assert!(!transport.origin().contains("SECRETKEY"));
        assert!(transport.origin().ends_with("/…"));
    }

    #[tokio::test]
    async fn a_rate_limit_phrased_as_a_json_rpc_error_is_retried() {
        let limited =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32007,"message":"request limit reached"}}"#;
        let ok = r#"{"jsonrpc":"2.0","id":1,"result":{"passphrase":"p"}}"#;
        let (url, server) = serve(vec![(429, "retry-after: 0\r\n", limited), (200, "", ok)]);
        let text = fast(&url).request("getNetwork", &json!({})).await.unwrap();
        assert_eq!(text, ok);
        assert_eq!(server.join().unwrap(), 2);
    }

    #[tokio::test]
    async fn recording_never_replaces_a_directory_that_is_not_a_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("notes");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("notes.txt"), "keep me").unwrap();
        let err = Recording::create(MemoryTransport::new(), &target, &hash())
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("not a fixture"), "{err}");
        assert_eq!(
            std::fs::read_to_string(target.join("notes.txt")).unwrap(),
            "keep me"
        );
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert!(
            Recording::create(MemoryTransport::new(), &file, &hash())
                .await
                .is_err()
        );
        // An empty directory is fine to record into.
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        Recording::create(MemoryTransport::new(), &empty, &hash())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_interrupted_commit_gets_its_old_fixture_back() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("rec");
        let backup = dir.path().join(".rec.backup");
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(backup.join(MANIFEST_FILE), "{}").unwrap();
        let recording = Recording::create(MemoryTransport::new(), &target, &hash())
            .await
            .unwrap();
        assert!(target.join(MANIFEST_FILE).exists());
        assert!(!backup.exists());
        recording.handle().discard().await;
        assert!(target.join(MANIFEST_FILE).exists(), "discarding keeps it");
    }

    #[tokio::test]
    async fn other_http_errors_are_not_retried() {
        let (url, server) = serve(vec![(404, "", "no such route")]);
        let err = fast(&url)
            .request("getNetwork", &json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 404"), "{err}");
        assert_eq!(server.join().unwrap(), 1);
    }

    #[tokio::test]
    async fn a_json_rpc_error_behind_an_http_error_is_read() {
        let envelope = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"bad params"}}"#;
        let (url, server) = serve(vec![(400, "", envelope)]);
        let rpc = crate::rpc::RpcClient::new(fast(&url));
        let err = rpc.network().await.unwrap_err();
        assert!(
            matches!(&err, RewindError::Rpc { code: Some(-32602), detail, .. } if detail == "bad params"),
            "{err}"
        );
        assert_eq!(server.join().unwrap(), 1);
    }

    #[tokio::test]
    async fn an_unreachable_rpc_says_why_and_hides_the_key() {
        // A port nothing listens on: bind one, then free it.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let transport = fast(&format!("http://127.0.0.1:{port}/v1/SECRETKEY"));
        let err = transport
            .request("getNetwork", &json!({}))
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, RewindError::RpcUnreachable { .. }), "{text}");
        assert!(text.contains("connection failed: "), "{text}");
        assert!(!text.contains("SECRETKEY"), "{text}");
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
        let target = dir.path().join("rec");
        let recording = Recording::create(live, &target, &hash()).await.unwrap();

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

        // Nothing exists at the target until the recording is committed.
        assert!(!target.exists());
        recording.handle().commit().await.unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["rec"], "no staging or backup directory is left");

        let manifest = FixtureManifest::load(&target).await.unwrap();
        assert_eq!(manifest.tx_hash().unwrap(), hash());

        // ...and replayed as one.
        let fixture = FixtureTransport::open(&target).await.unwrap();
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
        assert!(
            err.to_string().contains("it is not a recorded fixture"),
            "{err}"
        );
        let err = FixtureTransport::open(dir.path().join("typo"))
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("no such directory"), "{err}");

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

        let recording = Recording::create(MemoryTransport::new(), dir.path().join("rec"), &hash())
            .await
            .unwrap();
        recording.handle().commit().await.unwrap();
        let fixture = FixtureTransport::open(dir.path().join("rec"))
            .await
            .unwrap();
        let err = fixture
            .request("getLedgerEntries", &json!({"keys": ["never-recorded"]}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("ledger key never-recorded"),
            "{err}"
        );
        let err = fixture.request("getHealth", &json!({})).await.unwrap_err();
        assert!(matches!(err, RewindError::FixtureMissing { .. }), "{err}");
        assert!(err.to_string().contains("no recorded answer for getHealth"));

        // The fixture replays its own transaction, and no other.
        assert_eq!(fixture.hash_for(None).unwrap(), hash());
        assert_eq!(fixture.hash_for(Some(&hash())).unwrap(), hash());
        let other: TxHash = "cd".repeat(32).parse().unwrap();
        let err = fixture.hash_for(Some(&other)).unwrap_err();
        assert!(
            matches!(err, RewindError::FixtureHashMismatch { .. }),
            "{err}"
        );
    }

    #[tokio::test]
    async fn recording_failures_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let live = MemoryTransport::new().on("getNetwork", &json!({"result": {}}));
        let recording = Recording::create(live, dir.path().join("rec"), &hash())
            .await
            .unwrap();
        // Pull the staging directory out from under the recorder.
        tokio::fs::remove_dir_all(dir.path().join(".rec.recording"))
            .await
            .unwrap();
        let err = recording
            .request("getNetwork", &json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, RewindError::RecordIo { .. }), "{err}");
        // The real cause is kept, not replaced by a generic sentence.
        assert!(std::error::Error::source(&err).is_some());
    }

    async fn record(target: &Path, hash: &TxHash, passphrase: &str) -> RecordingHandle {
        let live = MemoryTransport::new()
            .on("getNetwork", &json!({"result": {"passphrase": passphrase}}))
            .on(
                "getLedgerEntries",
                &json!({"result": {"entries": [{"key": passphrase, "xdr": "e"}]}}),
            );
        let recording = Recording::create(live, target, hash).await.unwrap();
        recording.request("getNetwork", &json!({})).await.unwrap();
        recording
            .request("getLedgerEntries", &json!({"keys": [passphrase]}))
            .await
            .unwrap();
        recording.handle()
    }

    async fn recorded_passphrase(target: &Path) -> String {
        let fixture = FixtureTransport::open(target).await.unwrap();
        let text = fixture.request("getNetwork", &json!({})).await.unwrap();
        let response: Value = serde_json::from_str(&text).unwrap();
        response["result"]["passphrase"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn a_recording_replaces_a_fixture_whole_or_not_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("fixture.v1");
        let other: TxHash = "cd".repeat(32).parse().unwrap();

        record(&target, &hash(), "old")
            .await
            .commit()
            .await
            .unwrap();
        // A file only the old recording has.
        std::fs::write(target.join("getHealth.json"), "{}").unwrap();

        // A second recording that is abandoned leaves the old fixture alone.
        let abandoned = record(&target, &other, "abandoned").await;
        assert_eq!(recorded_passphrase(&target).await, "old");
        abandoned.discard().await;
        assert_eq!(recorded_passphrase(&target).await, "old");
        assert!(!dir.path().join(".fixture.v1.recording").exists());

        // A committed one replaces it entirely: nothing of the old one is left
        // to be mixed with the new transaction's responses.
        record(&target, &other, "new").await.commit().await.unwrap();
        assert_eq!(recorded_passphrase(&target).await, "new");
        let fixture = FixtureTransport::open(&target).await.unwrap();
        assert_eq!(fixture.tx_hash().unwrap(), other);
        assert!(!target.join("getHealth.json").exists());
        assert!(
            fixture
                .request("getLedgerEntries", &json!({"keys": ["old"]}))
                .await
                .is_err()
        );
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["fixture.v1"]);
        // No temporary file survives inside the fixture either.
        assert!(std::fs::read_dir(&target).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[tokio::test]
    async fn stale_staging_and_backup_directories_do_not_get_in_the_way() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.v2");
        // What an interrupted run leaves behind, with a file from another
        // transaction in it.
        for stale in [".a.v2.recording", ".a.v2.backup"] {
            std::fs::create_dir(dir.path().join(stale)).unwrap();
            std::fs::write(dir.path().join(stale).join("getHealth.json"), "{}").unwrap();
        }
        // A fixture whose name differs only after the dot is not disturbed.
        record(&dir.path().join("a.v1"), &hash(), "v1")
            .await
            .commit()
            .await
            .unwrap();

        record(&target, &hash(), "v2").await.commit().await.unwrap();
        assert_eq!(recorded_passphrase(&target).await, "v2");
        assert!(!target.join("getHealth.json").exists());
        assert_eq!(recorded_passphrase(&dir.path().join("a.v1")).await, "v1");
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["a.v1", "a.v2"]);
    }
}
