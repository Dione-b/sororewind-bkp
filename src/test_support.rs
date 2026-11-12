//! Builders for synthetic XDR values and RPC responses, shared by the unit
//! tests. Nothing here touches the network or a fixture.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use stellar_xdr::{
    AccountEntry, AccountId, ConfigSettingContractComputeV0, ConfigSettingContractEventsV0,
    ConfigSettingEntry, ConfigSettingId, ContractCodeEntry, ContractCodeEntryExt,
    ContractCostParams, ContractDataDurability, ContractDataEntry, ContractEvent,
    ContractEventBody, ContractEventType, ContractEventV0, ContractId, DiagnosticEvent,
    ExtensionPoint, FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
    FeeBumpTransactionInnerTx, Hash, HostFunction, InvokeContractArgs, InvokeHostFunctionOp,
    InvokeHostFunctionResult, LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerFootprint,
    LedgerKey, LedgerKeyAccount, LedgerKeyContractCode, Limits, Memo, MuxedAccount,
    MuxedAccountMed25519, Operation, OperationBody, OperationResult, OperationResultTr,
    Preconditions, PublicKey, ScAddress, ScError, ScSpecEntry, ScSpecUdtErrorEnumCaseV0,
    ScSpecUdtErrorEnumV0, ScString, ScSymbol, ScVal, ScVec, SequenceNumber, SorobanResources,
    SorobanResourcesExtV0, SorobanTransactionData, SorobanTransactionDataExt,
    StateArchivalSettings, Transaction, TransactionEnvelope, TransactionExt, TransactionResult,
    TransactionResultExt, TransactionResultResult, TransactionV1Envelope, Uint256, WriteXdr,
};

use crate::codec;
use crate::error::{Result, RewindError};
use crate::replay::{BudgetUse, Fidelity, PrngSeed};
use crate::report::{
    Diagnostics, FootprintReport, Report, ResultPair, ResultSummary, SCHEMA_VERSION, TtlDriftEntry,
    TtlDriftReason,
};
use crate::rpc::{RawLedgerEntry, RawTransaction, TxStatus};
use crate::story::Story;
use crate::transport::{BoxFuture, Transport};
use crate::tx::DecodedTx;

// --- transport --------------------------------------------------------------

type Responder = Box<dyn Fn(&Value) -> String + Send + Sync>;
pub type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// A transport that answers from memory and remembers what it was asked.
#[derive(Default)]
pub struct MemoryTransport {
    responders: HashMap<String, Responder>,
    calls: Calls,
}

impl MemoryTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer `method` with a fixed JSON document.
    pub fn on(self, method: &str, response: &Value) -> Self {
        self.on_text(method, &response.to_string())
    }

    /// Answer `method` with a raw body, which need not be JSON.
    pub fn on_text(mut self, method: &str, body: &str) -> Self {
        let body = body.to_string();
        self.responders
            .insert(method.to_string(), Box::new(move |_| body.clone()));
        self
    }

    /// Answer `method` with a document computed from the request params.
    pub fn on_fn(
        mut self,
        method: &str,
        respond: impl Fn(&Value) -> Value + Send + Sync + 'static,
    ) -> Self {
        self.responders.insert(
            method.to_string(),
            Box::new(move |params| respond(params).to_string()),
        );
        self
    }

    /// The `(method, params)` pairs requested so far.
    pub fn calls(&self) -> Calls {
        Arc::clone(&self.calls)
    }
}

impl Transport for MemoryTransport {
    fn request<'a>(&'a self, method: &'a str, params: &'a Value) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params.clone()));
            match self.responders.get(method) {
                Some(respond) => Ok(respond(params)),
                None => Err(RewindError::Rpc {
                    origin: self.origin(),
                    code: None,
                    method: method.to_string(),
                    detail: "no response configured".to_string(),
                }),
            }
        })
    }

    fn origin(&self) -> String {
        "memory".to_string()
    }
}

// --- scalars ----------------------------------------------------------------

pub fn sym(text: &str) -> ScVal {
    ScVal::Symbol(ScSymbol(text.try_into().unwrap()))
}

pub fn string(text: &str) -> ScVal {
    ScVal::String(ScString(text.try_into().unwrap()))
}

pub fn account(bytes: [u8; 32]) -> AccountId {
    AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
}

pub fn muxed(bytes: [u8; 32], id: u64) -> MuxedAccount {
    MuxedAccount::MuxedEd25519(MuxedAccountMed25519 {
        id,
        ed25519: Uint256(bytes),
    })
}

// --- diagnostic events ------------------------------------------------------

pub fn diag(
    contract: Option<[u8; 32]>,
    topics: Vec<ScVal>,
    data: ScVal,
    ok: bool,
) -> DiagnosticEvent {
    DiagnosticEvent {
        in_successful_contract_call: ok,
        event: ContractEvent {
            ext: ExtensionPoint::V0,
            contract_id: contract.map(|id| ContractId(Hash(id))),
            type_: ContractEventType::Diagnostic,
            body: ContractEventBody::V0(ContractEventV0 {
                topics: topics.try_into().unwrap(),
                data,
            }),
        },
    }
}

fn vec_of(items: Vec<ScVal>) -> ScVal {
    ScVal::Vec(Some(ScVec(items.try_into().unwrap())))
}

/// `fn_call` as the host emits it: from the caller, naming the callee.
pub fn fn_call(
    caller: Option<[u8; 32]>,
    callee: [u8; 32],
    name: &str,
    args: Vec<ScVal>,
) -> DiagnosticEvent {
    let callee = ScVal::Bytes(stellar_xdr::ScBytes(callee.to_vec().try_into().unwrap()));
    diag(
        caller,
        vec![sym("fn_call"), callee, sym(name)],
        vec_of(args),
        true,
    )
}

/// `fn_return` as the host emits it: from the contract that is returning.
pub fn fn_return(contract: [u8; 32], name: &str, value: ScVal) -> DiagnosticEvent {
    diag(
        Some(contract),
        vec![sym("fn_return"), sym(name)],
        value,
        true,
    )
}

pub fn error_event(contract: Option<[u8; 32]>, error: ScError, message: &str) -> DiagnosticEvent {
    diag(
        contract,
        vec![sym("error"), ScVal::Error(error)],
        vec_of(vec![string(message)]),
        false,
    )
}

pub fn log_event(contract: Option<[u8; 32]>, message: &str) -> DiagnosticEvent {
    diag(
        contract,
        vec![sym("log")],
        vec_of(vec![string(message)]),
        true,
    )
}

// --- ledger entries ---------------------------------------------------------

fn entry(data: LedgerEntryData, last_modified: u32) -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: last_modified,
        data,
        ext: LedgerEntryExt::V0,
    }
}

pub fn account_entry(id: [u8; 32], last_modified: u32) -> LedgerEntry {
    entry(
        LedgerEntryData::Account(AccountEntry {
            account_id: account(id),
            balance: 1_000,
            ..AccountEntry::default()
        }),
        last_modified,
    )
}

pub fn data_entry(
    contract: [u8; 32],
    durability: ContractDataDurability,
    last_modified: u32,
) -> LedgerEntry {
    entry(
        LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: ScAddress::Contract(ContractId(Hash(contract))),
            key: sym("Balance"),
            durability,
            val: ScVal::U32(1),
        }),
        last_modified,
    )
}

pub fn code_entry(wasm: &[u8], last_modified: u32) -> LedgerEntry {
    entry(
        LedgerEntryData::ContractCode(ContractCodeEntry {
            ext: ContractCodeEntryExt::V0,
            hash: Hash([0xcc; 32]),
            code: wasm.to_vec().try_into().unwrap(),
        }),
        last_modified,
    )
}

pub fn key_of(entry: &LedgerEntry) -> LedgerKey {
    entry.to_key()
}

/// An entry the way `getLedgerEntries` returns it.
pub fn raw_entry(entry: &LedgerEntry, live_until: Option<u32>) -> RawLedgerEntry {
    let b64 = |bytes: Vec<u8>| codec::b64_encode(&bytes);
    RawLedgerEntry {
        key: b64(entry.to_key().to_xdr(Limits::none()).unwrap()),
        xdr: b64(entry.data.to_xdr(Limits::none()).unwrap()),
        ext_xdr: Some(b64(entry.ext.to_xdr(Limits::none()).unwrap())),
        last_modified_ledger_seq: Some(entry.last_modified_ledger_seq),
        live_until_ledger_seq: live_until,
    }
}

/// A config setting entry with mainnet-like values.
pub fn config_entry(id: ConfigSettingId, last_modified: u32) -> RawLedgerEntry {
    let setting = match id {
        ConfigSettingId::ContractCostParamsCpuInstructions => {
            ConfigSettingEntry::ContractCostParamsCpuInstructions(ContractCostParams::default())
        }
        ConfigSettingId::ContractCostParamsMemoryBytes => {
            ConfigSettingEntry::ContractCostParamsMemoryBytes(ContractCostParams::default())
        }
        ConfigSettingId::ContractComputeV0 => {
            ConfigSettingEntry::ContractComputeV0(ConfigSettingContractComputeV0 {
                tx_memory_limit: 41_943_040,
                ..ConfigSettingContractComputeV0::default()
            })
        }
        ConfigSettingId::ContractEventsV0 => {
            ConfigSettingEntry::ContractEventsV0(ConfigSettingContractEventsV0 {
                tx_max_contract_events_size_bytes: 16_384,
                fee_contract_events1_kb: 0,
            })
        }
        ConfigSettingId::StateArchival => {
            ConfigSettingEntry::StateArchival(StateArchivalSettings {
                max_entry_ttl: 3_110_400,
                min_temporary_ttl: 17_280,
                min_persistent_ttl: 2_073_600,
                ..StateArchivalSettings::default()
            })
        }
        ConfigSettingId::ContractLedgerCostV0 => ConfigSettingEntry::ContractLedgerCostV0(
            stellar_xdr::ConfigSettingContractLedgerCostV0 {
                fee_disk_read_ledger_entry: 6_250,
                fee_write_ledger_entry: 10_000,
                fee_disk_read1_kb: 1_786,
                soroban_state_target_size_bytes: 3_000_000_000,
                rent_fee1_kb_soroban_state_size_low: -17_000,
                rent_fee1_kb_soroban_state_size_high: 10_000,
                soroban_state_rent_fee_growth_factor: 5_000,
                ..Default::default()
            },
        ),
        ConfigSettingId::ContractLedgerCostExtV0 => ConfigSettingEntry::ContractLedgerCostExtV0(
            stellar_xdr::ConfigSettingContractLedgerCostExtV0 {
                tx_max_footprint_entries: 100,
                fee_write1_kb: 3_500,
            },
        ),
        ConfigSettingId::ContractHistoricalDataV0 => ConfigSettingEntry::ContractHistoricalDataV0(
            stellar_xdr::ConfigSettingContractHistoricalDataV0 {
                fee_historical1_kb: 16_235,
            },
        ),
        ConfigSettingId::ContractBandwidthV0 => {
            ConfigSettingEntry::ContractBandwidthV0(stellar_xdr::ConfigSettingContractBandwidthV0 {
                fee_tx_size1_kb: 1_624,
                ..Default::default()
            })
        }
        ConfigSettingId::ContractMaxSizeBytes => ConfigSettingEntry::ContractMaxSizeBytes(131_072),
        ConfigSettingId::ContractDataEntrySizeBytes => {
            ConfigSettingEntry::ContractDataEntrySizeBytes(65_536)
        }
        ConfigSettingId::LiveSorobanStateSizeWindow => {
            ConfigSettingEntry::LiveSorobanStateSizeWindow(
                vec![1_000_000_000u64, 1_000_000_002, 1_000_000_003]
                    .try_into()
                    .unwrap(),
            )
        }
        other => panic!("no test value for {other}"),
    };
    raw_entry(
        &entry(LedgerEntryData::ConfigSetting(setting), last_modified),
        None,
    )
}

/// The settings `state::build_config` asks for, in request order.
pub fn config_entries(last_modified: u32) -> Vec<Option<RawLedgerEntry>> {
    [
        ConfigSettingId::ContractCostParamsCpuInstructions,
        ConfigSettingId::ContractCostParamsMemoryBytes,
        ConfigSettingId::ContractComputeV0,
        ConfigSettingId::ContractEventsV0,
        ConfigSettingId::StateArchival,
        ConfigSettingId::ContractLedgerCostV0,
        ConfigSettingId::ContractLedgerCostExtV0,
        ConfigSettingId::ContractHistoricalDataV0,
        ConfigSettingId::ContractBandwidthV0,
        ConfigSettingId::ContractMaxSizeBytes,
        ConfigSettingId::ContractDataEntrySizeBytes,
        ConfigSettingId::LiveSorobanStateSizeWindow,
    ]
    .into_iter()
    .map(|id| Some(config_entry(id, last_modified)))
    .collect()
}

// --- transactions -----------------------------------------------------------

/// The knobs of a synthetic Soroban transaction.
pub struct InvokeSpec {
    pub tx_source: MuxedAccount,
    pub op_source: Option<MuxedAccount>,
    /// Read-write footprint indices listed for automatic restoration.
    pub restored: Vec<u32>,
    /// Whether the transaction carries Soroban data at all.
    pub soroban: bool,
    pub extra_ops: Vec<Operation>,
    /// Use this operation instead of the host invocation.
    pub replace_op: Option<Operation>,
}

impl Default for InvokeSpec {
    fn default() -> Self {
        Self {
            tx_source: MuxedAccount::Ed25519(Uint256([1; 32])),
            op_source: None,
            restored: Vec::new(),
            soroban: true,
            extra_ops: Vec::new(),
            replace_op: None,
        }
    }
}

fn host_function() -> HostFunction {
    HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: ScAddress::Contract(ContractId(Hash([0xaa; 32]))),
        function_name: ScSymbol("run".try_into().unwrap()),
        args: Vec::new().try_into().unwrap(),
    })
}

fn resources() -> SorobanResources {
    SorobanResources {
        footprint: LedgerFootprint {
            read_only: vec![LedgerKey::ContractCode(LedgerKeyContractCode {
                hash: Hash([0xcc; 32]),
            })]
            .try_into()
            .unwrap(),
            read_write: vec![LedgerKey::Account(LedgerKeyAccount {
                account_id: account([1; 32]),
            })]
            .try_into()
            .unwrap(),
        },
        instructions: 1_000_000,
        disk_read_bytes: 1_000,
        write_bytes: 1_000,
    }
}

pub fn invoke_envelope(spec: &InvokeSpec) -> TransactionEnvelope {
    let invoke = Operation {
        source_account: spec.op_source.clone(),
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: host_function(),
            auth: Vec::new().try_into().unwrap(),
        }),
    };
    let mut operations = vec![spec.replace_op.clone().unwrap_or(invoke)];
    operations.extend(spec.extra_ops.iter().cloned());

    let ext = if spec.soroban {
        TransactionExt::V1(SorobanTransactionData {
            ext: if spec.restored.is_empty() {
                SorobanTransactionDataExt::V0
            } else {
                SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                    archived_soroban_entries: spec.restored.clone().try_into().unwrap(),
                })
            },
            resources: resources(),
            resource_fee: 100,
        })
    } else {
        TransactionExt::V0
    };

    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: Transaction {
            source_account: spec.tx_source.clone(),
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext,
        },
        signatures: Vec::new().try_into().unwrap(),
    })
}

/// Wrap a v1 envelope in a fee-bump paid by `fee_source`.
pub fn fee_bump(inner: TransactionEnvelope, fee_source: [u8; 32]) -> TransactionEnvelope {
    let TransactionEnvelope::Tx(inner) = inner else {
        panic!("only a v1 envelope can be fee-bumped");
    };
    TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: FeeBumpTransaction {
            fee_source: MuxedAccount::Ed25519(Uint256(fee_source)),
            fee: 1_000,
            inner_tx: FeeBumpTransactionInnerTx::Tx(inner),
            ext: FeeBumpTransactionExt::V0,
        },
        signatures: Vec::new().try_into().unwrap(),
    })
}

/// A base64 `TransactionResult` with the given outcome.
pub fn tx_result(result: TransactionResultResult) -> String {
    let result = TransactionResult {
        fee_charged: 100,
        result,
        ext: TransactionResultExt::V0,
    };
    codec::b64_encode(&result.to_xdr(Limits::none()).unwrap())
}

/// A base64 `TransactionResult` for a failed host invocation.
pub fn result_xdr(result: InvokeHostFunctionResult) -> String {
    let ops = vec![OperationResult::OpInner(
        OperationResultTr::InvokeHostFunction(result),
    )];
    tx_result(TransactionResultResult::TxFailed(ops.try_into().unwrap()))
}

/// A `getTransaction` record for a failed transaction in ledger 100.
pub fn raw_tx(envelope: &TransactionEnvelope, result_b64: &str) -> RawTransaction {
    RawTransaction {
        status: TxStatus::Failed,
        tx_hash: None,
        ledger: Some(100),
        application_order: Some(3),
        envelope_xdr: Some(codec::b64_encode(&envelope.to_xdr(Limits::none()).unwrap())),
        result_xdr: Some(result_b64.to_string()),
        result_meta_xdr: None,
    }
}

/// A decoded transaction that trapped on-chain, with no diagnostics.
pub fn decoded_tx() -> DecodedTx {
    DecodedTx {
        hash: "22".repeat(32).parse().unwrap(),
        ledger: 100,
        fee_bump: false,
        application_order: Some(1),
        source_account: account([1; 32]),
        host_function: host_function(),
        resources: resources(),
        resource_fee: 0,
        envelope_size: 0,
        auth: Vec::new(),
        restored_rw_indices: Vec::new(),
        onchain_result: InvokeHostFunctionResult::Trapped,
        onchain_diagnostic_events: Vec::new(),
    }
}

// --- WASM -------------------------------------------------------------------

/// A WASM binary made of the given `(section id, payload)` pairs.
pub fn wasm_with_sections(sections: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut wasm = b"\0asm\x01\0\0\0".to_vec();
    for (id, payload) in sections {
        wasm.push(*id);
        // The length, as unsigned LEB128.
        let mut length = payload.len();
        loop {
            let byte = (length & 0x7f) as u8;
            length >>= 7;
            if length == 0 {
                wasm.push(byte);
                break;
            }
            wasm.push(byte | 0x80);
        }
        wasm.extend(payload);
    }
    wasm
}

/// The body of a `contractspecv0` section declaring one error enum.
pub fn spec_section(enum_name: &str, cases: &[(&str, u32)]) -> Vec<u8> {
    let cases: Vec<ScSpecUdtErrorEnumCaseV0> = cases
        .iter()
        .map(|(name, value)| ScSpecUdtErrorEnumCaseV0 {
            doc: Default::default(),
            name: (*name).try_into().unwrap(),
            value: *value,
        })
        .collect();
    ScSpecEntry::UdtErrorEnumV0(ScSpecUdtErrorEnumV0 {
        doc: Default::default(),
        lib: Default::default(),
        name: enum_name.try_into().unwrap(),
        cases: cases.try_into().unwrap(),
    })
    .to_xdr(Limits::none())
    .unwrap()
}

// --- reports ----------------------------------------------------------------

/// A report exercising every part of the renderers: a nested failure, a
/// recovered call, logs, drift of both kinds, network budget figures.
pub fn sample_report() -> Report {
    const C: [u8; 32] = [0x0a; 32];
    let fatal = ScError::Contract(3);
    let events = [
        fn_call(None, C, "outer", vec![ScVal::U32(1), string("two")]),
        log_event(Some(C), "starting"),
        // A `try_call` that fails and is recovered from.
        fn_call(Some(C), C, "probe", vec![]),
        error_event(Some(C), ScError::Contract(9), "probe failed"),
        fn_call(Some(C), C, "inner", vec![]),
        log_event(Some(C), "about to fail"),
        error_event(Some(C), fatal.clone(), "balance too low"),
        error_event(Some(C), fatal.clone(), "escalating"),
    ];
    let mut story = Story::build(&events, Some(&fatal));
    if let Some(failure) = &mut story.failure {
        failure.name = Some("InsufficientBalance".to_string());
        failure.name_origin = Some("spec enum Error".to_string());
    }

    let trapped = ResultSummary::from(&InvokeHostFunctionResult::Trapped);
    let diagnostic = "fail [-] error, contract error #3 \"balance too low\"".to_string();
    Report {
        schema_version: SCHEMA_VERSION,
        hash: "a".repeat(64).parse().unwrap(),
        fee_bump: true,
        network: "testnet".to_string(),
        rpc: "https://rpc.example".to_string(),
        ledger: 1_000,
        protocol: 30,
        close_time: 1_700_000_000,
        status: "failed",
        fidelity: Fidelity::MATCH,
        result: ResultPair {
            onchain: trapped.clone(),
            local: trapped,
        },
        story,
        budget: BudgetUse {
            cpu_insns: 1_234_567,
            cpu_limit: 2_000_000,
            mem_bytes: 1_048_576,
            mem_limit: 41_943_040,
            network_cpu_insns: Some(1_234_567),
            network_mem_bytes: Some(1_048_576),
        },
        footprint: FootprintReport {
            entries: 4,
            missing: 1,
            drifted: vec!["account GAAA".to_string()],
            ttl_drifted: vec![TtlDriftEntry {
                entry: "contract data C…AAAA Balance (persistent)".to_string(),
                reason: TtlDriftReason::ExtendedSince,
            }],
            ..FootprintReport::default()
        },
        prng_seed: PrngSeed::Derived,
        diagnostics: Diagnostics {
            onchain: vec![diagnostic.clone()],
            local: vec![diagnostic],
            ..Diagnostics::default()
        },
    }
}
