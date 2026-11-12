//! decode: turn the raw RPC payload into the exact inputs the host needs.
//!
//! Nothing here talks to the network. The stage answers one question — *is
//! this a Soroban transaction, and what exactly did it ask the host to do?* —
//! and fails loudly when the answer is no.

use stellar_xdr::{
    AccountId, DiagnosticEvent, FeeBumpTransactionInnerTx, HostFunction,
    InnerTransactionResultResult, InvokeHostFunctionResult, LedgerKey, MuxedAccount, OperationBody,
    OperationResult, OperationResultTr, PublicKey, SorobanAuthorizationEntry, SorobanResources,
    SorobanTransactionDataExt, Transaction, TransactionEnvelope, TransactionExt, TransactionMeta,
    TransactionResult, TransactionResultResult,
};

use crate::codec;
use crate::error::{Result, RewindError};
use crate::hash::TxHash;
use crate::rpc::RawTransaction;

/// Everything replay needs, lifted out of the envelope.
#[derive(Debug)]
pub struct DecodedTx {
    /// The hash the transaction was fetched by.
    pub hash: TxHash,
    /// Sequence of the ledger the transaction was applied in.
    pub ledger: u32,
    /// The envelope was a fee-bump wrapping the Soroban transaction.
    pub fee_bump: bool,
    /// One-based position in the ledger's apply order, when the RPC reports
    /// it. Zero is not a position: its readers treat it as unknown.
    pub application_order: Option<u32>,

    /// The account the host treats as the transaction invoker.
    pub source_account: AccountId,
    /// What the `InvokeHostFunction` operation asked the host to run.
    pub host_function: HostFunction,
    /// The declared footprint and resource limits (`sorobanData.resources`).
    pub resources: SorobanResources,
    /// The resource fee the transaction declared (`sorobanData.resourceFee`).
    pub resource_fee: i64,
    /// XDR size of the Soroban transaction's own envelope, signatures
    /// included: what core charges the transaction-size fee on. For a
    /// fee-bump that is the inner envelope.
    pub envelope_size: u32,
    /// The authorization entries the operation carried, in envelope order.
    pub auth: Vec<SorobanAuthorizationEntry>,
    /// Indices into `resources.footprint.read_write` of entries the transaction
    /// asked the host to auto-restore. Empty for ordinary invocations.
    pub restored_rw_indices: Vec<u32>,

    /// What the network actually recorded.
    pub onchain_result: InvokeHostFunctionResult,
    /// The diagnostic events the network recorded; empty when the RPC sent
    /// no `resultMetaXdr` or the meta holds none.
    pub onchain_diagnostic_events: Vec<DiagnosticEvent>,
}

impl DecodedTx {
    /// Footprint keys in the order the host demands: read-only first, then
    /// read-write. `invoke_host_function` is positional about this.
    pub fn footprint_keys(&self) -> Vec<LedgerKey> {
        let footprint = &self.resources.footprint;
        footprint
            .read_only
            .iter()
            .chain(footprint.read_write.iter())
            .cloned()
            .collect()
    }

    /// The host function as raw XDR, for the host.
    pub fn encoded_host_function(&self) -> Result<Vec<u8>> {
        codec::encode(&self.host_function, "the host function")
    }

    /// The declared resources as raw XDR, for the host.
    pub fn encoded_resources(&self) -> Result<Vec<u8>> {
        codec::encode(&self.resources, "the transaction resources")
    }

    /// The source account as raw XDR, for the host.
    pub fn encoded_source_account(&self) -> Result<Vec<u8>> {
        codec::encode(&self.source_account, "the source account")
    }

    /// Each authorization entry as raw XDR, in envelope order.
    pub fn encoded_auth(&self) -> Result<Vec<Vec<u8>>> {
        self.auth
            .iter()
            .map(|entry| codec::encode(entry, "an authorization entry"))
            .collect()
    }
}

/// Decode a `getTransaction` record. Fails with `NotSoroban` unless the
/// envelope holds exactly one `InvokeHostFunction` operation with Soroban
/// resources, and with `MissingField` when the RPC left out a field it needs.
pub fn decode(hash: &TxHash, raw: &RawTransaction) -> Result<DecodedTx> {
    let not_soroban = |reason: String| RewindError::NotSoroban {
        hash: hash.clone(),
        reason,
    };
    fn field<'a>(value: &'a Option<String>, field: &'static str) -> Result<&'a str> {
        value.as_deref().ok_or(RewindError::MissingField {
            method: "getTransaction",
            field,
        })
    }

    let envelope: TransactionEnvelope =
        codec::decode(field(&raw.envelope_xdr, "envelopeXdr")?, "envelopeXdr")?;

    // A fee-bump only changes who pays: the Soroban transaction is its inner
    // transaction, and that is what the host executes.
    let (inner, fee_bump) = match envelope {
        TransactionEnvelope::Tx(v1) => (v1, false),
        TransactionEnvelope::TxFeeBump(outer) => match outer.tx.inner_tx {
            FeeBumpTransactionInnerTx::Tx(inner) => (inner, true),
        },
        TransactionEnvelope::TxV0(_) => {
            return Err(not_soroban(
                "it is a legacy v0 envelope, which cannot carry Soroban resources or a footprint"
                    .to_string(),
            ));
        }
    };
    // A `TransactionEnvelope` is its four-byte type followed by the envelope.
    let envelope_size = 4 + codec::encode(&inner, "the transaction envelope")?.len();
    let tx = inner.tx;
    let Transaction {
        source_account: tx_source,
        operations,
        ext,
        ..
    } = tx;

    let TransactionExt::V1(soroban_data) = ext else {
        return Err(not_soroban(
            "its `tx.ext` is empty, so it carries no Soroban resources or footprint".to_string(),
        ));
    };

    let [op] = operations.as_slice() else {
        return Err(not_soroban(format!(
            "it has {} operations; Soroban transactions must have exactly one",
            operations.len()
        )));
    };
    let OperationBody::InvokeHostFunction(invoke) = &op.body else {
        return Err(not_soroban(format!(
            "its operation is `{}`, not `InvokeHostFunction`",
            op.body.name()
        )));
    };

    let ledger = raw.ledger.ok_or(RewindError::MissingField {
        method: "getTransaction",
        field: "ledger",
    })?;

    let tx_result: TransactionResult =
        codec::decode(field(&raw.result_xdr, "resultXdr")?, "resultXdr")?;
    let onchain_result = extract_invoke_result(&tx_result, hash)?;

    let onchain_diagnostic_events = match raw.result_meta_xdr.as_deref() {
        Some(meta) => diagnostic_events(&codec::decode(meta, "resultMetaXdr")?),
        None => Vec::new(),
    };

    let restored_rw_indices = match &soroban_data.ext {
        SorobanTransactionDataExt::V1(ext) => ext.archived_soroban_entries.to_vec(),
        SorobanTransactionDataExt::V0 => Vec::new(),
    };

    // "Transaction invoker" authorization resolves to the source account of
    // the operation when it has one, and of the transaction otherwise.
    let source_account = account_id(op.source_account.as_ref().unwrap_or(&tx_source));

    Ok(DecodedTx {
        hash: hash.clone(),
        ledger,
        fee_bump,
        application_order: raw.application_order,
        source_account,
        host_function: invoke.host_function.clone(),
        resources: soroban_data.resources,
        resource_fee: soroban_data.resource_fee,
        envelope_size: u32::try_from(envelope_size).unwrap_or(u32::MAX),
        auth: invoke.auth.to_vec(),
        restored_rw_indices,
        onchain_result,
        onchain_diagnostic_events,
    })
}

/// The envelope addresses accounts as `MuxedAccount`; the host wants the
/// underlying `AccountId`. A muxed address is the same account with a memo-like
/// id attached, so dropping the id loses nothing the host cares about.
fn account_id(account: &MuxedAccount) -> AccountId {
    let key = match account {
        MuxedAccount::Ed25519(key) => key.clone(),
        MuxedAccount::MuxedEd25519(muxed) => muxed.ed25519.clone(),
    };
    AccountId(PublicKey::PublicKeyTypeEd25519(key))
}

/// Pull the `InvokeHostFunctionResult` out of the transaction result.
fn extract_invoke_result(
    result: &TransactionResult,
    hash: &TxHash,
) -> Result<InvokeHostFunctionResult> {
    let never_reached_host = |code: &'static str| RewindError::NeverReachedHost {
        hash: hash.clone(),
        code,
    };

    let ops: &[OperationResult] = match &result.result {
        TransactionResultResult::TxSuccess(ops) | TransactionResultResult::TxFailed(ops) => ops,
        // A fee-bump reports the Soroban transaction's outcome one level down.
        TransactionResultResult::TxFeeBumpInnerSuccess(pair)
        | TransactionResultResult::TxFeeBumpInnerFailed(pair) => match &pair.result.result {
            InnerTransactionResultResult::TxSuccess(ops)
            | InnerTransactionResultResult::TxFailed(ops) => ops,
            other => return Err(never_reached_host(other.name())),
        },
        other => return Err(never_reached_host(other.name())),
    };

    match ops.first() {
        Some(OperationResult::OpInner(OperationResultTr::InvokeHostFunction(r))) => Ok(r.clone()),
        Some(OperationResult::OpInner(other)) => Err(RewindError::NotSoroban {
            hash: hash.clone(),
            reason: format!(
                "the recorded operation result is `{}`, not a host function result",
                other.name()
            ),
        }),
        Some(other) => Err(never_reached_host(other.name())),
        None => Err(RewindError::MissingField {
            method: "getTransaction",
            field: "resultXdr operation results",
        }),
    }
}

fn diagnostic_events(meta: &TransactionMeta) -> Vec<DiagnosticEvent> {
    match meta {
        TransactionMeta::V3(v3) => v3
            .soroban_meta
            .as_ref()
            .map(|meta| meta.diagnostic_events.to_vec())
            .unwrap_or_default(),
        TransactionMeta::V4(v4) => v4.diagnostic_events.to_vec(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::TxStatus;
    use crate::test_support::{
        InvokeSpec, account, fee_bump, invoke_envelope, muxed, raw_tx, result_xdr, tx_result,
    };
    use stellar_xdr::{
        InnerTransactionResult, InnerTransactionResultExt, InnerTransactionResultPair,
        InnerTransactionResultResult as Inner, InvokeHostFunctionResult as Invoke, Operation,
        PaymentOp, TransactionResultExt, TransactionV0Envelope,
    };

    fn hash() -> TxHash {
        "11".repeat(32).parse().unwrap()
    }

    fn decode_spec(spec: &InvokeSpec) -> Result<DecodedTx> {
        let raw = raw_tx(&invoke_envelope(spec), &result_xdr(Invoke::Trapped));
        decode(&hash(), &raw)
    }

    #[test]
    fn decodes_a_plain_invocation() {
        let tx = decode_spec(&InvokeSpec::default()).unwrap();
        assert_eq!(tx.ledger, 100);
        assert!(!tx.fee_bump);
        assert_eq!(tx.application_order, Some(3));
        assert_eq!(tx.source_account, account([1; 32]));
        assert_eq!(tx.onchain_result, Invoke::Trapped);
        assert!(tx.restored_rw_indices.is_empty());
        assert_eq!(tx.footprint_keys().len(), 2, "read-only then read-write");
        assert!(!tx.encoded_host_function().unwrap().is_empty());
    }

    #[test]
    fn operation_source_overrides_transaction_source() {
        // No operation source: the transaction source is the invoker.
        let tx = decode_spec(&InvokeSpec::default()).unwrap();
        assert_eq!(tx.source_account, account([1; 32]));

        // Plain operation source.
        let tx = decode_spec(&InvokeSpec {
            op_source: Some(MuxedAccount::Ed25519(stellar_xdr::Uint256([2; 32]))),
            ..InvokeSpec::default()
        })
        .unwrap();
        assert_eq!(tx.source_account, account([2; 32]));

        // Muxed operation source resolves to its underlying account.
        let tx = decode_spec(&InvokeSpec {
            op_source: Some(muxed([3; 32], 42)),
            ..InvokeSpec::default()
        })
        .unwrap();
        assert_eq!(tx.source_account, account([3; 32]));

        // Muxed transaction source, no operation source.
        let tx = decode_spec(&InvokeSpec {
            tx_source: muxed([4; 32], 7),
            ..InvokeSpec::default()
        })
        .unwrap();
        assert_eq!(tx.source_account, account([4; 32]));
    }

    #[test]
    fn unwraps_fee_bumps() {
        let spec = InvokeSpec {
            op_source: Some(MuxedAccount::Ed25519(stellar_xdr::Uint256([2; 32]))),
            restored: vec![0],
            ..InvokeSpec::default()
        };
        let inner_ops = vec![OperationResult::OpInner(
            OperationResultTr::InvokeHostFunction(Invoke::ResourceLimitExceeded),
        )];
        let outer = TransactionResult {
            fee_charged: 200,
            result: TransactionResultResult::TxFeeBumpInnerFailed(InnerTransactionResultPair {
                transaction_hash: stellar_xdr::Hash([0; 32]),
                result: InnerTransactionResult {
                    fee_charged: 100,
                    result: Inner::TxFailed(inner_ops.try_into().unwrap()),
                    ext: InnerTransactionResultExt::V0,
                },
            }),
            ext: TransactionResultExt::V0,
        };
        let raw = raw_tx(
            &fee_bump(invoke_envelope(&spec), [9; 32]),
            &crate::codec::b64_encode(&crate::codec::encode(&outer, "result").unwrap()),
        );
        let tx = decode(&hash(), &raw).unwrap();
        assert!(tx.fee_bump);
        // The invoker is the inner operation's source, never the fee source.
        assert_eq!(tx.source_account, account([2; 32]));
        assert_eq!(tx.onchain_result, Invoke::ResourceLimitExceeded);
        assert_eq!(tx.restored_rw_indices, [0]);
    }

    #[test]
    fn fee_bump_inner_validation_failure_never_reached_the_host() {
        let outer = TransactionResult {
            fee_charged: 200,
            result: TransactionResultResult::TxFeeBumpInnerFailed(InnerTransactionResultPair {
                transaction_hash: stellar_xdr::Hash([0; 32]),
                result: InnerTransactionResult {
                    fee_charged: 0,
                    result: Inner::TxBadSeq,
                    ext: InnerTransactionResultExt::V0,
                },
            }),
            ext: TransactionResultExt::V0,
        };
        let raw = raw_tx(
            &fee_bump(invoke_envelope(&InvokeSpec::default()), [9; 32]),
            &crate::codec::b64_encode(&crate::codec::encode(&outer, "result").unwrap()),
        );
        let err = decode(&hash(), &raw).unwrap_err();
        assert!(
            matches!(
                err,
                RewindError::NeverReachedHost {
                    code: "TxBadSeq",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn transaction_level_failures_are_not_called_successes() {
        let raw = raw_tx(
            &invoke_envelope(&InvokeSpec::default()),
            &tx_result(TransactionResultResult::TxBadSeq),
        );
        let err = decode(&hash(), &raw).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("failed before it reached the Soroban host"),
            "{text}"
        );
        assert!(text.contains("`TxBadSeq`"), "{text}");
        assert!(!text.contains("succeeded"), "{text}");

        let raw = raw_tx(
            &invoke_envelope(&InvokeSpec::default()),
            &tx_result(TransactionResultResult::TxFailed(
                vec![OperationResult::OpBadAuth].try_into().unwrap(),
            )),
        );
        let err = decode(&hash(), &raw).unwrap_err();
        assert!(
            matches!(
                err,
                RewindError::NeverReachedHost {
                    code: "OpBadAuth",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn rejects_everything_that_is_not_a_single_host_invocation() {
        let reason = |raw: RawTransaction| match decode(&hash(), &raw).unwrap_err() {
            RewindError::NotSoroban { reason, .. } => reason,
            other => panic!("expected NotSoroban, got {other}"),
        };
        let trapped = result_xdr(Invoke::Trapped);

        let v0 = TransactionEnvelope::TxV0(TransactionV0Envelope::default());
        assert!(reason(raw_tx(&v0, &trapped)).contains("legacy v0 envelope"));

        let classic = invoke_envelope(&InvokeSpec {
            soroban: false,
            ..InvokeSpec::default()
        });
        assert!(reason(raw_tx(&classic, &trapped)).contains("`tx.ext` is empty"));

        let payment = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp::default()),
        };
        let two_ops = invoke_envelope(&InvokeSpec {
            extra_ops: vec![payment.clone()],
            ..InvokeSpec::default()
        });
        assert!(reason(raw_tx(&two_ops, &trapped)).contains("it has 2 operations"));

        let not_invoke = invoke_envelope(&InvokeSpec {
            replace_op: Some(payment),
            ..InvokeSpec::default()
        });
        assert!(
            reason(raw_tx(&not_invoke, &trapped)).contains("`Payment`, not `InvokeHostFunction`")
        );

        let wrong_result = tx_result(TransactionResultResult::TxFailed(
            vec![OperationResult::OpInner(OperationResultTr::Payment(
                stellar_xdr::PaymentResult::Underfunded,
            ))]
            .try_into()
            .unwrap(),
        ));
        assert!(
            reason(raw_tx(
                &invoke_envelope(&InvokeSpec::default()),
                &wrong_result
            ))
            .contains("`Payment`, not a host function result")
        );
    }

    #[test]
    fn missing_fields_are_named() {
        let mut raw = raw_tx(
            &invoke_envelope(&InvokeSpec::default()),
            &result_xdr(Invoke::Trapped),
        );
        raw.ledger = None;
        let err = decode(&hash(), &raw).unwrap_err().to_string();
        assert_eq!(err, "the getTransaction response is missing `ledger`");

        let raw = RawTransaction {
            status: TxStatus::Failed,
            tx_hash: None,
            ledger: Some(1),
            application_order: None,
            envelope_xdr: None,
            result_xdr: None,
            result_meta_xdr: None,
        };
        let err = decode(&hash(), &raw).unwrap_err().to_string();
        assert_eq!(err, "the getTransaction response is missing `envelopeXdr`");
    }
}
