//! Every error a user can run into, through the whole pipeline.
//!
//! Each test puts a recorded fixture into a state the network can produce
//! (see `common::Overlay`) and checks three things: the error variant, the
//! exit code, and the words the user reads. Nothing here touches the network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;

use serde_json::{Value, json};
use sororewind::rpc::RpcClient;
use sororewind::transport::{FixtureTransport, HttpTransport};
use sororewind::{Request, RewindError, codec};
use stellar_xdr::{
    ConfigSettingId, LedgerHeaderHistoryEntry, LedgerKey, Limits, Operation, OperationBody,
    PaymentOp, TransactionEnvelope, TransactionResult, TransactionResultExt,
    TransactionResultResult, WriteXdr,
};

use common::Overlay;

const FIXTURE: &str = "contract-error";

/// Replay `request` through `rpc`, expect a failure, and check that it exits
/// with code 1 and says `words`.
async fn fails_with(rpc: &RpcClient, request: &Request, words: &str) -> RewindError {
    let result = sororewind::run(rpc, request).await;
    assert_eq!(sororewind::exit_code(&result), 1, "{result:?}");
    let err = result.expect_err("the replay should have failed");
    let message = err.to_string();
    assert!(
        message.contains(words),
        "expected {words:?} in the message:\n{message}"
    );
    err
}

async fn fails(overlay: Overlay, words: &str) -> RewindError {
    fails_with(&overlay.client(), &common::request(FIXTURE).await, words).await
}

fn b64<T: WriteXdr>(value: &T) -> String {
    codec::b64_encode(&value.to_xdr(Limits::none()).unwrap())
}

/// Rewrite `field` of the recorded `getTransaction` result.
fn transaction(field: &'static str, value: Value) -> impl Fn(&mut Value) + Send + Sync + 'static {
    move |result| result[field] = value.clone()
}

/// Drop the ledger entries `drop` selects from the recorded response.
fn without_entries(
    drop: impl Fn(&LedgerKey) -> bool + Send + Sync + 'static,
) -> impl Fn(&mut Value) + Send + Sync + 'static {
    move |result| {
        result["entries"].as_array_mut().unwrap().retain(|entry| {
            let key: LedgerKey = codec::decode(entry["key"].as_str().unwrap(), "key").unwrap();
            !drop(&key)
        });
    }
}

#[tokio::test]
async fn transaction_not_found() {
    let overlay = Overlay::on(FIXTURE)
        .await
        .rewrite_result("getTransaction", |result| {
            *result = json!({ "status": "NOT_FOUND", "latestLedger": 5_673_661 });
        })
        .respond(
            "getHealth",
            json!({ "jsonrpc": "2.0", "id": 1, "result": {
                "status": "healthy",
                "latestLedger": 5_673_661,
                "oldestLedger": 5_552_702,
                "ledgerRetentionWindow": 120_960,
            }}),
        );
    let err = fails(overlay, "not found on testnet").await;
    assert!(matches!(err, RewindError::NotFound { .. }));
    // The retention window the RPC reported is part of the explanation.
    assert!(err.to_string().contains("120960 ledgers"), "{err}");
}

#[tokio::test]
async fn transaction_not_found_without_a_health_probe() {
    // The fixture never recorded `getHealth`, so the probe fails; that only
    // changes the wording.
    let overlay = Overlay::on(FIXTURE)
        .await
        .rewrite_result("getTransaction", |result| {
            *result = json!({ "status": "NOT_FOUND" });
        });
    let err = fails(overlay, "about 7 days").await;
    assert!(matches!(err, RewindError::NotFound { .. }));
}

#[tokio::test]
async fn transaction_that_succeeded() {
    let overlay = Overlay::on(FIXTURE)
        .await
        .rewrite_result("getTransaction", transaction("status", json!("SUCCESS")));
    let err = fails(overlay, "succeeded on-chain").await;
    assert!(matches!(err, RewindError::NotAFailure { .. }));
}

#[tokio::test]
async fn transaction_that_is_not_soroban() {
    let overlay = Overlay::on(FIXTURE)
        .await
        .rewrite_result("getTransaction", |result| {
            let envelope: TransactionEnvelope =
                codec::decode(result["envelopeXdr"].as_str().unwrap(), "envelopeXdr").unwrap();
            let TransactionEnvelope::Tx(mut v1) = envelope else {
                panic!("the fixture's envelope is not a v1 transaction");
            };
            v1.tx.operations = vec![Operation {
                source_account: None,
                body: OperationBody::Payment(PaymentOp {
                    destination: v1.tx.source_account.clone(),
                    asset: stellar_xdr::Asset::Native,
                    amount: 1,
                }),
            }]
            .try_into()
            .unwrap();
            result["envelopeXdr"] = b64(&TransactionEnvelope::Tx(v1)).into();
        });
    let err = fails(overlay, "is not a Soroban transaction").await;
    assert!(matches!(err, RewindError::NotSoroban { .. }));
}

#[tokio::test]
async fn transaction_that_never_reached_the_host() {
    let overlay = Overlay::on(FIXTURE)
        .await
        .rewrite_result("getTransaction", |result| {
            let bad_seq = TransactionResult {
                fee_charged: 100,
                result: TransactionResultResult::TxBadSeq,
                ext: TransactionResultExt::V0,
            };
            result["resultXdr"] = b64(&bad_seq).into();
        });
    let err = fails(overlay, "failed before it reached the Soroban host").await;
    assert!(matches!(err, RewindError::NeverReachedHost { .. }));
}

#[tokio::test]
async fn transaction_from_another_protocol() {
    // A protocol newer than any embedded host.
    let unsupported = sororewind::host::newest().protocol + 1;
    let overlay = Overlay::on(FIXTURE)
        .await
        .rewrite_result("getLedgers", move |result| {
            let ledger = &mut result["ledgers"][0];
            let mut history: LedgerHeaderHistoryEntry =
                codec::decode(ledger["headerXdr"].as_str().unwrap(), "headerXdr").unwrap();
            history.header.ledger_version = unsupported;
            ledger["headerXdr"] = b64(&history).into();
        });
    let err = fails(overlay, &format!("ran on protocol {unsupported}")).await;
    assert!(matches!(err, RewindError::ProtocolMismatch { .. }));
}

#[tokio::test]
async fn ledger_outside_the_retention_window() {
    let overlay = Overlay::on(FIXTURE)
        .await
        .rewrite_result("getLedgers", |result| result["ledgers"] = json!([]));
    let err = fails(overlay, "no ledger 5672381 in its retention window").await;
    assert!(matches!(err, RewindError::LedgerUnavailable { .. }));
}

#[tokio::test]
async fn network_setting_missing() {
    let overlay = Overlay::on(FIXTURE).await.rewrite_result(
        "getLedgerEntries",
        without_entries(|key| {
            matches!(key, LedgerKey::ConfigSetting(setting)
                if setting.config_setting_id == ConfigSettingId::StateArchival)
        }),
    );
    let err = fails(overlay, "did not return the network setting").await;
    assert!(matches!(err, RewindError::ConfigSettingMissing { .. }));
}

#[tokio::test]
async fn durable_entry_without_a_ttl() {
    let overlay = Overlay::on(FIXTURE)
        .await
        .rewrite_result("getLedgerEntries", |result| {
            let entry = result["entries"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|entry| {
                    let key: LedgerKey =
                        codec::decode(entry["key"].as_str().unwrap(), "key").unwrap();
                    matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_))
                })
                .expect("the fixture has no contract entry");
            entry.as_object_mut().unwrap().remove("liveUntilLedgerSeq");
        });
    let err = fails(overlay, "without").await;
    assert!(matches!(err, RewindError::MissingTtl { .. }), "{err:?}");
}

#[tokio::test]
async fn rpc_error_response() {
    let overlay = Overlay::on(FIXTURE).await.respond(
        "getTransaction",
        json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32602, "message": "invalid hash" } }),
    );
    let err = fails(overlay, "invalid hash").await;
    // The JSON-RPC code is kept, and shown.
    assert!(matches!(
        err,
        RewindError::Rpc {
            code: Some(-32602),
            ..
        }
    ));
    assert!(err.to_string().ends_with("(JSON-RPC code -32602)"), "{err}");
}

#[tokio::test]
async fn transaction_with_an_unknown_status() {
    let overlay = Overlay::on(FIXTURE)
        .await
        .rewrite_result("getTransaction", transaction("status", json!("PENDING")));
    let err = fails(overlay, "a status this build of sororewind does not know").await;
    assert!(matches!(err, RewindError::Rpc { .. }));
}

#[tokio::test]
async fn ledger_outside_the_rpc_window() {
    // What stellar-rpc answers for a `startLedger` it no longer keeps.
    let overlay = Overlay::on(FIXTURE).await.respond(
        "getLedgers",
        json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32600, "message":
            "start ledger (5) must be between the oldest ledger: 5559289 and the latest \
             ledger: 5680248 for this rpc instance" } }),
    );
    let err = fails(overlay, "in its retention window").await;
    assert!(
        matches!(err, RewindError::LedgerUnavailable { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn rpc_response_missing_a_field() {
    let overlay = Overlay::on(FIXTURE)
        .await
        .rewrite_result("getTransaction", |result| {
            result.as_object_mut().unwrap().remove("envelopeXdr");
        });
    let err = fails(overlay, "missing `envelopeXdr`").await;
    assert!(matches!(err, RewindError::MissingField { .. }));
}

#[tokio::test]
async fn rpc_response_with_undecodable_xdr() {
    let overlay = Overlay::on(FIXTURE)
        .await
        .rewrite_result("getTransaction", transaction("envelopeXdr", json!("AAAA")));
    let err = fails(overlay, "failed to decode").await;
    assert!(matches!(err, RewindError::XdrDecode { .. }));
}

#[tokio::test]
async fn unreachable_rpc() {
    // Nothing listens on the discard port; the connection is refused at once.
    // It is retried first; keep the waits short.
    let rpc = RpcClient::new(
        HttpTransport::new("http://127.0.0.1:9")
            .unwrap()
            .with_retry_base(std::time::Duration::from_millis(1)),
    );
    let err = fails_with(
        &rpc,
        &common::request(FIXTURE).await,
        "cannot reach the RPC",
    )
    .await;
    assert!(matches!(err, RewindError::RpcUnreachable { .. }));
}

#[tokio::test]
async fn wasm_debug_file_that_does_not_exist() {
    let mut request = common::request(FIXTURE).await;
    request.wasm_debug = Some(PathBuf::from("/nonexistent/contract.wasm"));
    let rpc = Overlay::on(FIXTURE).await.client();
    let err = fails_with(&rpc, &request, "could not read --wasm-debug").await;
    assert!(matches!(err, RewindError::WasmFile { .. }));
}

#[tokio::test]
async fn fixture_directory_that_does_not_exist() {
    let err = FixtureTransport::open("/nonexistent/fixture")
        .await
        .err()
        .unwrap();
    assert!(matches!(err, RewindError::FixtureInvalid { .. }));
    assert!(err.to_string().contains("no such directory"), "{err}");
    assert_eq!(sororewind::exit_code(&Err(err)), 1);
}

#[tokio::test]
async fn fixture_in_another_layout() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("fixture.json"),
        r#"{"version": 1, "hash": "00"}"#,
    )
    .unwrap();
    let err = FixtureTransport::open(dir.path()).await.err().unwrap();
    assert!(err.to_string().contains("re-record it"), "{err}");
    assert!(matches!(err, RewindError::FixtureInvalid { .. }));
    assert_eq!(sororewind::exit_code(&Err(err)), 1);
}

#[tokio::test]
async fn fixture_missing_a_response() {
    // A fixture recorded for another transaction has no answer for this one.
    let request = Request {
        hash: "ab".repeat(32).parse().unwrap(),
        network: None,
        wasm_debug: None,
    };
    let rpc = RpcClient::new(
        FixtureTransport::open(common::fixture_dir(FIXTURE))
            .await
            .unwrap(),
    );
    let err = fails_with(&rpc, &request, "getTransaction").await;
    assert!(matches!(err, RewindError::FixtureMissing { .. }));
}
