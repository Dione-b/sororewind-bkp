// The host-facing half of a replay, compiled once per embedded host: `p29.rs`
// and `p30.rs` each mount this file with `soroban_env_host` bound to their
// own host. Only the API every embedded host shares may be used here, and no
// host type may cross into the rest of the crate; see `host/mod.rs`.

use super::soroban_env_host::{
    budget::Budget,
    e2e_invoke::{entry_size_for_rent, invoke_host_function, TtlLedgerEntryMeta},
    storage::Storage,
    vm::VersionedContractCodeCostInputs,
    xdr::{
        ContractCodeCostInputs, ContractCostParams, Hash, LedgerEntry, Limits, ReadXdr, ScError,
        WriteXdr,
    },
    Host, HostError, LedgerInfo, ModuleCache,
};
use crate::host::{Change, Failure, Invocation, Output};

/// The protocol this host executes, and the only one it accepts.
pub const PROTOCOL: u32 = super::soroban_env_host::meta::INTERFACE_VERSION.protocol;

/// This host's `soroban-env-host` version.
pub const VERSION: &str = super::soroban_env_host::VERSION.pkg;

fn xdr(e: impl std::fmt::Display) -> Failure {
    Failure::Xdr(e.to_string())
}

pub fn invoke(invocation: &Invocation<'_>) -> Result<Output, Failure> {
    let cpu_params =
        ContractCostParams::from_xdr(&invocation.cpu_cost_params, Limits::none()).map_err(xdr)?;
    let mem_params =
        ContractCostParams::from_xdr(&invocation.mem_cost_params, Limits::none()).map_err(xdr)?;

    // Cost parameters come from the ledger's own ConfigSetting entries so the
    // host prices every instruction exactly as the network did.
    let budget = Budget::try_from_configs(
        invocation.cpu_limit,
        invocation.mem_limit,
        cpu_params.clone(),
        mem_params.clone(),
    )
    .map_err(|e| Failure::Budget(e.to_string()))?;
    let module_cache = module_cache(invocation, cpu_params, mem_params)?;

    let entries = invocation
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let Some(entry) = entry else {
                return Ok((None, None));
            };
            let ttl = match entry.live_until {
                None => None,
                Some(live_until_ledger) => Some(TtlLedgerEntryMeta {
                    live_until_ledger,
                    entry_size_for_rent: rent_size(&entry.xdr).ok_or(Failure::Rent(index))?,
                }),
            };
            Ok((Some(entry.xdr.as_slice()), ttl))
        })
        .collect::<Result<Vec<_>, Failure>>()?;

    let ledger = &invocation.ledger;
    let ledger_info = LedgerInfo {
        protocol_version: ledger.protocol_version,
        sequence_number: ledger.sequence_number,
        timestamp: ledger.timestamp,
        network_id: ledger.network_id,
        base_reserve: ledger.base_reserve,
        min_temp_entry_ttl: ledger.min_temp_entry_ttl,
        min_persistent_entry_ttl: ledger.min_persistent_entry_ttl,
        max_entry_ttl: ledger.max_entry_ttl,
    };

    let mut diagnostics = Vec::new();
    let output = invoke_host_function(
        &budget,
        true,
        invocation.host_function.as_slice(),
        invocation.resources.as_slice(),
        invocation.restored_rw_indices,
        invocation.source_account.as_slice(),
        invocation.auth.iter().map(Vec::as_slice),
        ledger_info,
        entries.into_iter(),
        invocation.prng_seed.as_slice(),
        &mut diagnostics,
        None,
        Some(module_cache),
    )
    .map_err(|e| Failure::Start(e.to_string()))?;

    let result = match &output.encoded_invoke_result {
        Ok(value) => Ok(value.clone()),
        Err(error) => Err(match ScError::try_from(error) {
            Ok(error) => Some(error.to_xdr(Limits::none()).map_err(xdr)?),
            Err(_) => None,
        }),
    };
    Ok(Output {
        result,
        changes: output
            .ledger_changes
            .into_iter()
            .map(|change| Change {
                read_only: change.read_only,
                new_value: change.encoded_new_value,
            })
            .collect(),
        contract_events: output.encoded_contract_events,
        diagnostics: diagnostics
            .iter()
            .map(|event| event.to_xdr(Limits::none()).map_err(xdr))
            .collect::<Result<_, _>>()?,
        cpu_insns: budget.get_cpu_insns_consumed().unwrap_or_default(),
        mem_bytes: budget.get_mem_bytes_consumed().unwrap_or_default(),
    })
}

/// The size the host charges rent on: the encoded entry, plus the parsed
/// module's memory for contract code.
fn rent_size(entry_xdr: &[u8]) -> Option<u32> {
    let entry = LedgerEntry::from_xdr(entry_xdr, Limits::none()).ok()?;
    let size = u32::try_from(entry_xdr.len()).ok()?;
    entry_size_for_rent(&Budget::default(), &entry, size).ok()
}

/// Pre-parse every contract in the footprint, the way stellar-core does.
///
/// Since protocol 23 core keeps all contract code parsed in a module cache,
/// so an invocation is only charged for *instantiating* a module. Without the
/// cache the host parses each module inside the invocation and charges for it,
/// which inflates the instruction count by an order of a million per contract
/// and can push a replay over a limit the network never hit.
fn module_cache(
    invocation: &Invocation<'_>,
    cpu_params: ContractCostParams,
    mem_params: ContractCostParams,
) -> Result<ModuleCache, Failure> {
    let setup = |e: HostError| Failure::ModuleCache(e.to_string());

    // Parsing is not charged to the transaction; give it room to work.
    let budget =
        Budget::try_from_configs(u64::MAX, u64::MAX, cpu_params, mem_params).map_err(setup)?;
    let context = Host::with_storage_and_budget(Storage::default(), budget);
    let cache = ModuleCache::new(&context).map_err(setup)?;

    for contract in &invocation.contracts {
        let cost_inputs = match &contract.cost_inputs {
            None => VersionedContractCodeCostInputs::V0 {
                wasm_bytes: contract.wasm.len(),
            },
            Some(inputs) => VersionedContractCodeCostInputs::V1(
                ContractCodeCostInputs::from_xdr(inputs, Limits::none()).map_err(xdr)?,
            ),
        };
        // A module that does not parse is simply not cached; the host reports
        // the problem itself if the transaction goes on to call it.
        if let Err(e) = cache.parse_and_cache_module(
            &context,
            invocation.ledger.protocol_version,
            &Hash(contract.hash),
            &contract.wasm,
            cost_inputs,
        ) {
            log::debug!(target: "replay", "module {} not cached: {e}", hex::encode(contract.hash));
        }
    }
    Ok(cache)
}
