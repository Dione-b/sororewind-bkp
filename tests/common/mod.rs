//! Helpers shared by the integration tests.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sororewind::rpc::RpcClient;
use sororewind::transport::{BoxFuture, FixtureManifest, FixtureTransport, Transport};
use sororewind::{Report, Request, Result, TxHash};

pub fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

pub fn fixture_dir(name: &str) -> PathBuf {
    fixtures_root().join(name)
}

/// The transaction a fixture was recorded for.
pub async fn fixture_hash(name: &str) -> TxHash {
    FixtureManifest::load(&fixture_dir(name))
        .await
        .unwrap()
        .tx_hash()
        .unwrap()
}

pub async fn request(name: &str) -> Request {
    Request {
        hash: fixture_hash(name).await,
        network: None,
        wasm_debug: None,
    }
}

/// Replay a recorded fixture in-process.
pub async fn replay(name: &str) -> Result<Report> {
    let rpc = RpcClient::new(FixtureTransport::open(fixture_dir(name)).await?);
    sororewind::run(&rpc, &request(name).await).await
}

/// What the binary would print and exit with, in the shape the snapshots
/// have always had. The fixture directory is redacted so snapshots do not
/// depend on where the repository lives.
pub fn transcript(result: &Result<Report>, body: &str) -> String {
    let root = fixtures_root().display().to_string();
    format!("--- exit code: {}\n{body}", sororewind::exit_code(result)).replace(&root, "<fixtures>")
}

type Rewrite = Box<dyn Fn(Value) -> Value + Send + Sync>;

/// A recorded fixture with some responses rewritten: the cheapest way to put
/// the pipeline in a situation no recording covers.
pub struct Overlay {
    inner: FixtureTransport,
    rewrites: HashMap<&'static str, Rewrite>,
}

impl Overlay {
    pub async fn on(name: &str) -> Self {
        Self {
            inner: FixtureTransport::open(fixture_dir(name)).await.unwrap(),
            rewrites: HashMap::new(),
        }
    }

    /// Replace the whole JSON-RPC response of `method`.
    pub fn respond(self, method: &'static str, response: Value) -> Self {
        self.rewrite(method, move |_| response.clone())
    }

    /// Transform the recorded JSON-RPC response of `method`. A method the
    /// fixture has no recording for is handed `null`.
    pub fn rewrite(
        mut self,
        method: &'static str,
        rewrite: impl Fn(Value) -> Value + Send + Sync + 'static,
    ) -> Self {
        self.rewrites.insert(method, Box::new(rewrite));
        self
    }

    /// Transform the `result` object of the recorded response of `method`.
    pub fn rewrite_result(
        self,
        method: &'static str,
        edit: impl Fn(&mut Value) + Send + Sync + 'static,
    ) -> Self {
        self.rewrite(method, move |mut response| {
            edit(&mut response["result"]);
            response
        })
    }

    pub fn client(self) -> RpcClient {
        RpcClient::new(self)
    }
}

impl Transport for Overlay {
    fn request<'a>(&'a self, method: &'a str, params: &'a Value) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let Some(rewrite) = self.rewrites.get(method) else {
                return self.inner.request(method, params).await;
            };
            let recorded = match self.inner.request(method, params).await {
                Ok(text) => serde_json::from_str(&text).unwrap_or(Value::Null),
                Err(_) => Value::Null,
            };
            Ok(rewrite(recorded).to_string())
        })
    }

    fn origin(&self) -> String {
        "overlay".to_string()
    }
}
