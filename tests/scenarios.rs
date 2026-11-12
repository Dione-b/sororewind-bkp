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
    ConfigSettingEntry, ConfigSettingId, LedgerEntryData, LedgerKeyConfigSetting, TransactionExt,
};
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

/// Rewrite `getTransaction` as if core had failed the operation with `code`
/// before the host ran, so there are no diagnostics.
fn failed_before_the_host(result: &mut Value, code: InvokeHostFunctionResult) {
    let plain: TransactionResult =
        codec::decode(result["resultXdr"].as_str().unwrap(), "resultXdr").unwrap();
    let failed = TransactionResult {
        fee_charged: plain.fee_charged,
        result: TransactionResultResult::TxFailed(
            vec![OperationResult::OpInner(
                OperationResultTr::InvokeHostFunction(code),
            )]
            .try_into()
            .unwrap(),
        ),
        ext: TransactionResultExt::V0,
    };
    result["resultXdr"] = b64(&failed).into();
    // The diagnostics live in the transaction meta.
    result.as_object_mut().unwrap().remove("resultMetaXdr");
}

/// Rewrite `getTransaction` as if core had refused an archived entry.
fn failed_on_archived_entry(result: &mut Value) {
    failed_before_the_host(result, InvokeHostFunctionResult::EntryArchived);
}

/// Rewrite `getTransaction` as if the transaction had declared no bytes to
/// read from disk, and core had stopped at its first classic entry.
fn declared_no_disk_reads(result: &mut Value) {
    let envelope: TransactionEnvelope =
        codec::decode(result["envelopeXdr"].as_str().unwrap(), "envelopeXdr").unwrap();
    let TransactionEnvelope::Tx(mut envelope) = envelope else {
        panic!("the fixture's envelope is not a v1 transaction");
    };
    let TransactionExt::V1(data) = &mut envelope.tx.ext else {
        panic!("the fixture's transaction carries no Soroban data");
    };
    data.resources.disk_read_bytes = 0;
    result["envelopeXdr"] = b64(&TransactionEnvelope::Tx(envelope)).into();
    failed_before_the_host(result, InvokeHostFunctionResult::ResourceLimitExceeded);
}

#[tokio::test]
async fn classic_entries_count_against_the_declared_disk_reads() {
    let report = replay(
        Overlay::on(FIXTURE)
            .await
            .rewrite_result("getTransaction", declared_no_disk_reads),
    )
    .await;

    assert_eq!(report.outcome(), Outcome::Matched);
    assert_eq!(report.result.local.code, "resource_limit_exceeded");
    let failure = report.story.failure.as_ref().unwrap();
    assert!(
        failure
            .message
            .contains("operation byte-read resources exceeds amount specified"),
        "{failure:?}"
    );
}

/// Rewrite `getLedgerEntries` as if the network took contract data entries of
/// one byte at most.
fn tiny_contract_data_limit(result: &mut Value) {
    let key = b64(&LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: ConfigSettingId::ContractDataEntrySizeBytes,
    }));
    for entry in result["entries"].as_array_mut().unwrap() {
        if entry["key"] == key.as_str() {
            entry["xdr"] = b64(&LedgerEntryData::ConfigSetting(
                ConfigSettingEntry::ContractDataEntrySizeBytes(1),
            ))
            .into();
        }
    }
}

#[tokio::test]
async fn an_entry_over_the_network_size_limit_fails_like_core() {
    let report = replay(
        Overlay::on(FIXTURE)
            .await
            .rewrite_result("getLedgerEntries", tiny_contract_data_limit)
            .rewrite_result("getTransaction", |result| {
                failed_before_the_host(result, InvokeHostFunctionResult::ResourceLimitExceeded);
            }),
    )
    .await;

    assert_eq!(report.outcome(), Outcome::Matched);
    assert_eq!(report.result.local.code, "resource_limit_exceeded");
    let failure = report.story.failure.as_ref().unwrap();
    assert!(
        failure
            .message
            .contains("ContractData size exceeds network config maximum size"),
        "{failure:?}"
    );
}

#[tokio::test]
async fn the_refundable_fee_check_is_mirrored() {
    // The host succeeds; core then finds the rent beyond the refundable fee.
    let report = common::replay("refundable-fee").await.unwrap();
    assert_eq!(report.outcome(), Outcome::Matched);
    assert_eq!(report.result.local.code, "insufficient_refundable_fee");
    let failure = report.story.failure.as_ref().unwrap();
    assert!(
        failure
            .message
            .contains("refundable resource fee was not sufficient"),
        "{failure:?}"
    );
    // Rent is priced from a window that keeps moving; the report says so.
    assert!(
        report
            .footprint
            .config_drifted
            .iter()
            .any(|setting| setting == "LiveSorobanStateSizeWindow")
    );
}

#[tokio::test]
async fn a_drifted_entry_is_rebuilt_from_ledger_metadata() {
    let report = common::replay("rebuilt-entry").await.unwrap();
    assert_eq!(report.outcome(), Outcome::Matched);
    assert!(!report.footprint.rebuilt.is_empty());
    assert!(report.footprint.drifted.is_empty());
    assert_eq!(report.footprint.rebuild_note, None);
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

#[tokio::test]
async fn each_embedded_host_replays_its_own_protocol() {
    // One recording per embedded host; both must match the network.
    for (fixture, protocol) in [("mainnet-auth-error", 29), ("contract-error", 30)] {
        let report = common::replay(fixture).await.unwrap();
        assert_eq!(report.protocol, protocol, "{fixture}");
        assert!(sororewind::host::for_protocol(protocol).is_some());
        assert_eq!(report.outcome(), Outcome::Matched, "{fixture}");
    }
}

/// A WASM whose `contractspecv0` section declares one error enum.
fn wasm_declaring(enum_name: &str, cases: &[(&str, u32)]) -> Vec<u8> {
    use stellar_xdr::{ScSpecEntry, ScSpecUdtErrorEnumCaseV0, ScSpecUdtErrorEnumV0};
    let spec = ScSpecEntry::UdtErrorEnumV0(ScSpecUdtErrorEnumV0 {
        doc: Default::default(),
        lib: Default::default(),
        name: enum_name.try_into().unwrap(),
        cases: cases
            .iter()
            .map(|(name, value)| ScSpecUdtErrorEnumCaseV0 {
                doc: Default::default(),
                name: (*name).try_into().unwrap(),
                value: *value,
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap(),
    })
    .to_xdr(Limits::none())
    .unwrap();
    let payload = [&[14u8][..], b"contractspecv0", &spec].concat();
    assert!(payload.len() < 128, "one LEB128 byte of length");
    [
        b"\0asm\x01\0\0\0",
        &[0u8, payload.len() as u8][..],
        &payload,
    ]
    .concat()
}

/// A custom account's `__check_auth` rejected the signature.
const CUSTOM_ACCOUNT: &str = "auth-check-auth";

#[tokio::test]
async fn a_contract_error_is_named_from_the_contract_on_the_ledger() {
    // The whole lookup, end to end: the failing contract's instance names its
    // code, the code's spec names the error.
    let report = common::replay(CUSTOM_ACCOUNT).await.unwrap();
    assert_eq!(report.outcome(), Outcome::Matched);
    let failure = report.story.failure.as_ref().unwrap();
    // The host reports an auth error; the contract error behind it is the
    // cause, in the account contract's own frame.
    assert_eq!(failure.error_type, "auth");
    assert_eq!(failure.name, None);
    let cause = failure.cause.as_ref().unwrap();
    assert_eq!(cause.function.as_deref(), Some("__check_auth"));
    assert!(cause.name.is_some(), "{cause:?}");
    let origin = cause.name_origin.as_deref().unwrap();
    assert!(
        origin.starts_with("spec enum ")
            && origin.ends_with("from the contract's code on the ledger"),
        "{origin}"
    );
    let frame = &report.story.calls[failure.call_index.unwrap()];
    assert!(frame.is_failure && frame.function == "__check_auth");
}

#[tokio::test]
async fn wasm_debug_names_the_error_from_a_local_file() {
    let ledger = common::replay(CUSTOM_ACCOUNT).await.unwrap();
    let code = ledger
        .story
        .failure
        .as_ref()
        .unwrap()
        .cause
        .as_ref()
        .unwrap()
        .contract_code;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("account.wasm");
    std::fs::write(
        &path,
        wasm_declaring("LocalError", &[("FromTheFile", code)]),
    )
    .unwrap();
    let mut request = common::request(CUSTOM_ACCOUNT).await;
    request.wasm_debug = Some(path.clone());
    let rpc = Overlay::on(CUSTOM_ACCOUNT).await.client();
    let report = sororewind::run(&rpc, &request).await.unwrap();

    let cause = report
        .story
        .failure
        .as_ref()
        .unwrap()
        .cause
        .as_ref()
        .unwrap();
    assert_eq!(cause.name.as_deref(), Some("FromTheFile"));
    assert_eq!(
        cause.name_origin.as_deref(),
        Some(format!("spec enum LocalError, from --wasm-debug {}", path.display()).as_str())
    );

    // A file that does not declare the code names nothing, and claims nothing.
    std::fs::write(&path, wasm_declaring("LocalError", &[("Other", code + 1)])).unwrap();
    let rpc = Overlay::on(CUSTOM_ACCOUNT).await.client();
    let report = sororewind::run(&rpc, &request).await.unwrap();
    let cause = report
        .story
        .failure
        .as_ref()
        .unwrap()
        .cause
        .as_ref()
        .unwrap();
    assert_eq!(
        (cause.name.as_deref(), cause.name_origin.as_deref()),
        (None, None)
    );
}

#[tokio::test]
async fn the_stellar_asset_contract_error_is_named() {
    let report = common::replay(FIXTURE).await.unwrap();
    let failure = report.story.failure.as_ref().unwrap();
    assert_eq!(failure.name.as_deref(), Some("BalanceError"));
    // The message keeps the values the contract reported.
    assert!(failure.message.contains('['), "{}", failure.message);
}
