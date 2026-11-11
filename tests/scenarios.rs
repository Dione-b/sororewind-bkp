//! End-to-end replays of situations no recording covers.
//!
//! Each test takes a recorded fixture and rewrites some of its RPC responses
//! (see `common::Overlay`), then runs the whole pipeline offline. The
//! rewrites stay consistent with what the network would have recorded, so a
//! correct replay must still match.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use serde_json::Value;
use sororewind::codec;
use sororewind::report::Report;
use sororewind::{Network, Outcome, RewindError};
use stellar_xdr::{
    ContractDataDurability, FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
    FeeBumpTransactionInnerTx, Hash, InnerTransactionResult, InnerTransactionResultExt,
    InnerTransactionResultPair, InnerTransactionResultResult, InvokeHostFunctionResult, LedgerKey,
    Limits, MuxedAccount, OperationResult, OperationResultTr, TransactionEnvelope,
    TransactionResult, TransactionResultExt, TransactionResultResult, Uint256, WriteXdr,
};

use common::Overlay;

/// A fixture whose failure is an ordinary contract error.
const FIXTURE: &str = "contract-error";

async fn replay(overlay: Overlay) -> Report {
    let request = common::request(FIXTURE).await;
    sororewind::run(&overlay.client(), &request)
        .await
        .expect("the rewritten fixture could not be replayed")
}

fn b64<T: WriteXdr>(value: &T) -> String {
    codec::b64_encode(&value.to_xdr(Limits::none()).unwrap())
}

/// The operation results of a plain `TransactionResult`.
fn operation_results(result: &TransactionResult) -> Vec<OperationResult> {
    match &result.result {
        TransactionResultResult::TxFailed(ops) | TransactionResultResult::TxSuccess(ops) => {
            ops.to_vec()
        }
        other => panic!("the fixture's result is not a plain one: {other:?}"),
    }
}

/// Rewrite `getTransaction` as if the transaction had been sponsored by a
/// fee-bump: the envelope is wrapped, and the result moves one level down.
fn as_fee_bump(result: &mut Value) {
    let inner: TransactionEnvelope =
        codec::decode(result["envelopeXdr"].as_str().unwrap(), "envelopeXdr").unwrap();
    let TransactionEnvelope::Tx(inner) = inner else {
        panic!("the fixture's envelope is not a v1 transaction");
    };
    let envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: FeeBumpTransaction {
            fee_source: MuxedAccount::Ed25519(Uint256([7; 32])),
            fee: inner.tx.fee as i64 * 2,
            inner_tx: FeeBumpTransactionInnerTx::Tx(inner),
            ext: FeeBumpTransactionExt::V0,
        },
        signatures: Vec::new().try_into().unwrap(),
    });

    let plain: TransactionResult =
        codec::decode(result["resultXdr"].as_str().unwrap(), "resultXdr").unwrap();
    let bumped = TransactionResult {
        fee_charged: plain.fee_charged,
        result: TransactionResultResult::TxFeeBumpInnerFailed(InnerTransactionResultPair {
            transaction_hash: Hash([1; 32]),
            result: InnerTransactionResult {
                fee_charged: plain.fee_charged,
                result: InnerTransactionResultResult::TxFailed(
                    operation_results(&plain).try_into().unwrap(),
                ),
                ext: InnerTransactionResultExt::V0,
            },
        }),
        ext: TransactionResultExt::V0,
    };

    result["envelopeXdr"] = b64(&envelope).into();
    result["resultXdr"] = b64(&bumped).into();
    result["feeBump"] = true.into();
}

#[tokio::test]
async fn a_fee_bumped_transaction_replays_like_a_plain_one() {
    let plain = replay(Overlay::on(FIXTURE).await).await;
    let bumped = replay(
        Overlay::on(FIXTURE)
            .await
            .rewrite_result("getTransaction", as_fee_bump),
    )
    .await;

    assert_eq!(bumped.outcome(), Outcome::Matched);
    assert!(bumped.fee_bump);
    assert!(!plain.fee_bump);
    // Only the fee payer changed: the host ran the same inner transaction.
    assert_eq!(bumped.result.local.code, plain.result.local.code);
    assert_eq!(bumped.story.calls.len(), plain.story.calls.len());
    let (bumped_failure, plain_failure) = (
        bumped.story.failure.as_ref().unwrap(),
        plain.story.failure.as_ref().unwrap(),
    );
    assert_eq!(bumped_failure.description, plain_failure.description);
    assert_eq!(bumped_failure.function, plain_failure.function);
}

/// The first persistent contract-data entry in the fixture's footprint.
fn persistent_entry_key() -> String {
    let path = common::fixture_dir(FIXTURE).join("ledgerEntries.json");
    let store: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    store["entries"]
        .as_object()
        .unwrap()
        .keys()
        .find(|key| {
            let key: LedgerKey = codec::decode(key, "a ledger key").unwrap();
            matches!(
                key,
                LedgerKey::ContractData(data) if data.durability == ContractDataDurability::Persistent
            )
        })
        .expect("the fixture's footprint has no persistent contract data")
        .clone()
}

/// Rewrite `getLedgerEntries` as if `key` had been archived since.
fn archive(key: String) -> impl Fn(&mut Value) + Send + Sync + 'static {
    move |result| {
        for entry in result["entries"].as_array_mut().unwrap() {
            if entry["key"] == key.as_str() {
                entry["liveUntilLedgerSeq"] = 0.into();
            }
        }
    }
}

/// Rewrite `getTransaction` as if core had refused an archived entry: the
/// operation fails with `ENTRY_ARCHIVED` and the host never ran, so there are
/// no diagnostics.
fn failed_on_archived_entry(result: &mut Value) {
    let plain: TransactionResult =
        codec::decode(result["resultXdr"].as_str().unwrap(), "resultXdr").unwrap();
    let archived = TransactionResult {
        fee_charged: plain.fee_charged,
        result: TransactionResultResult::TxFailed(
            vec![OperationResult::OpInner(
                OperationResultTr::InvokeHostFunction(InvokeHostFunctionResult::EntryArchived),
            )]
            .try_into()
            .unwrap(),
        ),
        ext: TransactionResultExt::V0,
    };
    result["resultXdr"] = b64(&archived).into();
    // The diagnostics live in the transaction meta.
    result.as_object_mut().unwrap().remove("resultMetaXdr");
}

#[tokio::test]
async fn an_archived_entry_fails_like_core() {
    let key = persistent_entry_key();
    let report = replay(
        Overlay::on(FIXTURE)
            .await
            .rewrite_result("getLedgerEntries", archive(key))
            .rewrite_result("getTransaction", failed_on_archived_entry),
    )
    .await;

    assert_eq!(report.outcome(), Outcome::Matched);
    assert_eq!(report.result.local.code, "entry_archived");
    assert_eq!(report.footprint.archived.len(), 1);
    let failure = report.story.failure.as_ref().unwrap();
    assert!(failure.message.contains("archived"), "{failure:?}");
}

#[tokio::test]
async fn an_entry_archived_since_the_transaction_is_ttl_drift() {
    // The network ran the host, so the entry was live back then: it is
    // replayed as live and reported as TTL drift, not as a failure.
    let key = persistent_entry_key();
    let report = replay(
        Overlay::on(FIXTURE)
            .await
            .rewrite_result("getLedgerEntries", archive(key)),
    )
    .await;

    assert_eq!(report.outcome(), Outcome::Matched);
    assert!(report.footprint.archived.is_empty());
    assert_eq!(report.footprint.ttl_drifted.len(), 1);
}

#[tokio::test]
async fn a_network_the_rpc_does_not_serve_is_an_error() {
    // The fixture is a testnet recording; asking for mainnet must not produce
    // a report labelled "mainnet".
    let mut request = common::request(FIXTURE).await;
    request.network = Some(Network::Mainnet);
    let result = sororewind::run(&Overlay::on(FIXTURE).await.client(), &request).await;
    assert!(
        matches!(result, Err(RewindError::NetworkMismatch { .. })),
        "{result:?}"
    );

    request.network = Some(Network::Testnet);
    let report = sororewind::run(&Overlay::on(FIXTURE).await.client(), &request)
        .await
        .unwrap();
    assert_eq!(report.network, "testnet");
}
