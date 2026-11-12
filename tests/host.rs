//! The embedded hosts, called directly through `sororewind::host`.
//!
//! The fixtures exercise each host end to end; these tests pin the adapter's
//! own contract (`src/host/proto.rs`): what crosses the boundary when the
//! host fails before it runs anything, and that every embedded host behaves
//! the same way.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use serde_json::Value;
use sororewind::codec;
use sororewind::host::{self, Failure, Invocation, LedgerInfo};
use stellar_xdr::{
    AccountId, ConfigSettingEntry, ConfigSettingId, ContractCostParams, ContractId, Hash,
    HostFunction, InvokeContractArgs, LedgerEntryData, LedgerFootprint, LedgerKey,
    LedgerKeyConfigSetting, Limits, PublicKey, ReadXdr, ScAddress, ScError, ScErrorCode, ScSymbol,
    ScVal, SorobanResources, Uint256, WriteXdr,
};

fn xdr<T: WriteXdr>(value: &T) -> Vec<u8> {
    value.to_xdr(Limits::none()).unwrap()
}

/// A network's real cost parameters, from a fixture recorded on `protocol`:
/// a host refuses a cost table that is not its own protocol's.
fn cost_params(protocol: u32, id: ConfigSettingId) -> Vec<u8> {
    let fixture = match protocol {
        29 => "mainnet-auth-error",
        30 => "contract-error",
        other => panic!("no fixture was recorded on protocol {other}"),
    };
    let path = common::fixture_dir(fixture).join("ledgerEntries.json");
    let store: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let key = codec::b64_encode(&xdr(&LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: id,
    })));
    let entry: LedgerEntryData =
        codec::decode(store["entries"][&key]["xdr"].as_str().unwrap(), "a setting").unwrap();
    let params: ContractCostParams = match entry {
        LedgerEntryData::ConfigSetting(
            ConfigSettingEntry::ContractCostParamsCpuInstructions(params)
            | ConfigSettingEntry::ContractCostParamsMemoryBytes(params),
        ) => params,
        other => panic!("not cost parameters: {other:?}"),
    };
    xdr(&params)
}

/// An invocation for the host of protocol `host`, in a ledger of `protocol`.
fn invocation_in(
    host: u32,
    protocol: u32,
    host_function: Vec<u8>,
    cpu_limit: u64,
) -> Invocation<'static> {
    Invocation {
        host_function,
        resources: xdr(&SorobanResources {
            footprint: LedgerFootprint::default(),
            instructions: u32::try_from(cpu_limit).unwrap(),
            disk_read_bytes: 0,
            write_bytes: 0,
        }),
        source_account: xdr(&AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            [1; 32],
        )))),
        auth: Vec::new(),
        restored_rw_indices: &[],
        ledger: LedgerInfo {
            protocol_version: protocol,
            sequence_number: 1_000,
            timestamp: 1_700_000_000,
            network_id: [7; 32],
            base_reserve: 5_000_000,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4_096,
            max_entry_ttl: 3_110_400,
        },
        entries: Vec::new(),
        prng_seed: [0; 32],
        cpu_limit,
        mem_limit: 40 * 1024 * 1024,
        cpu_cost_params: cost_params(host, ConfigSettingId::ContractCostParamsCpuInstructions),
        mem_cost_params: cost_params(host, ConfigSettingId::ContractCostParamsMemoryBytes),
        contracts: Vec::new(),
    }
}

fn invocation(protocol: u32, host_function: Vec<u8>, cpu_limit: u64) -> Invocation<'static> {
    invocation_in(protocol, protocol, host_function, cpu_limit)
}

/// A call to a contract that is not in the (empty) footprint.
fn call_to_nowhere() -> Vec<u8> {
    xdr(&HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: ScAddress::Contract(ContractId(Hash([9; 32]))),
        function_name: ScSymbol("hello".try_into().unwrap()),
        args: vec![ScVal::U32(1)].try_into().unwrap(),
    }))
}

fn error_of(output: &host::Output) -> ScError {
    let bytes = output
        .result
        .as_ref()
        .expect_err("the invocation cannot succeed")
        .as_deref()
        .expect("the host error carries an ScError");
    ScError::from_xdr(bytes, Limits::none()).unwrap()
}

#[test]
fn a_failure_inside_the_invocation_is_an_output_not_an_error() {
    for host in &host::HOSTS {
        let output = host
            .invoke(&invocation(host.protocol, call_to_nowhere(), 10_000_000))
            .unwrap_or_else(|failure| panic!("{host:?}: {failure:?}"));
        // Outside the footprint: a storage error, with the budget it cost.
        assert_eq!(
            error_of(&output),
            ScError::Storage(ScErrorCode::ExceededLimit),
            "{host:?}"
        );
        assert!(
            output.cpu_insns > 0 && output.cpu_insns < 10_000_000,
            "{host:?}"
        );
        assert!(output.changes.is_empty() && output.contract_events.is_empty());
        assert!(!output.diagnostics.is_empty(), "{host:?}");
    }
}

/// stellar-core records a failure while the host decodes its inputs as a
/// result (`TRAPPED` or `RESOURCE_LIMIT_EXCEEDED`), with the budget consumed;
/// it is not a reason to abort the replay.
#[test]
fn a_failure_before_the_invocation_is_an_output_too() {
    for host in &host::HOSTS {
        // Not enough instructions to even decode the inputs.
        let output = host
            .invoke(&invocation(host.protocol, call_to_nowhere(), 1))
            .unwrap_or_else(|failure| panic!("{host:?}: {failure:?}"));
        assert_eq!(
            error_of(&output),
            ScError::Budget(ScErrorCode::ExceededLimit),
            "{host:?}"
        );
        assert!(output.cpu_insns > 1, "{host:?}: the overrun is reported");

        // A host function that is not XDR at all.
        let output = host
            .invoke(&invocation(host.protocol, vec![0xff; 7], 10_000_000))
            .unwrap_or_else(|failure| panic!("{host:?}: {failure:?}"));
        assert!(output.result.is_err(), "{host:?}");
        assert!(output.cpu_insns > 0, "{host:?}");
    }
}

#[test]
fn a_host_only_runs_its_own_protocol() {
    for host in &host::HOSTS {
        let other = host.protocol + 1;
        // The host refuses the ledger, as a failed run or as a refusal to
        // start; either way it does not execute it.
        match host.invoke(&invocation_in(
            host.protocol,
            other,
            call_to_nowhere(),
            10_000_000,
        )) {
            Ok(output) => assert!(output.result.is_err(), "{host:?}"),
            Err(failure) => assert!(
                matches!(failure, Failure::Start(_)),
                "{host:?}: {failure:?}"
            ),
        }
    }
}

#[test]
fn unusable_cost_parameters_are_a_setup_failure() {
    for host in &host::HOSTS {
        let mut broken = invocation(host.protocol, call_to_nowhere(), 10_000_000);
        broken.cpu_cost_params = vec![0xff; 5];
        let failure = host.invoke(&broken).unwrap_err();
        assert!(matches!(failure, Failure::Xdr(_)), "{host:?}: {failure:?}");
    }
}
