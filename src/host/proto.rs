// The host-facing half of a replay, compiled once per embedded host: `p29.rs`
// and `p30.rs` each mount this file with `soroban_env_host` bound to their
// own host. Only the API every embedded host shares may be used here, and no
// host type may cross into the rest of the crate; see `host/mod.rs`.

use super::soroban_env_host::{
    Host, HostError, LedgerInfo, ModuleCache,
    budget::Budget,
    e2e_invoke::{
        TtlLedgerEntryMeta, entry_size_for_rent, extract_rent_changes, invoke_host_function,
    },
    fees::{
        FeeConfiguration, LedgerEntryRentChange, RentFeeConfiguration, RentWriteFeeConfiguration,
        TransactionResources, compute_rent_fee, compute_rent_write_fee_per_1kb,
        compute_transaction_resource_fee,
    },
    storage::Storage,
    vm::VersionedContractCodeCostInputs,
    xdr::{
        ContractCodeCostInputs, ContractCostParams, Hash, LedgerEntry, Limits, ReadXdr, ScError,
        ScErrorCode, ScErrorType, WriteXdr,
    },
};
use crate::host::{Change, Failure, FeeRequest, Fees, Invocation, Output, RentChange};

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
    // Rent is sized, like the module cache is filled, outside the
    // transaction's budget but with the network's own cost parameters: for
    // contract code the size includes the parsed module's memory.
    let unmetered =
        Budget::try_from_configs(u64::MAX, u64::MAX, cpu_params.clone(), mem_params.clone())
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
                    entry_size_for_rent: rent_size(&unmetered, entry.xdr)
                        .ok_or(Failure::Rent(index))?,
                }),
            };
            Ok((Some(entry.xdr), ttl))
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
    let invoked = invoke_host_function(
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
    );

    // The host decodes its inputs against the transaction's budget before it
    // runs anything, so it can fail there too. stellar-core (the bridge's
    // `run_invoke_and_build_output`, checked 2026-11-11) treats that exactly
    // like a failure inside the invocation: `success: false` with the budget
    // consumed so far. Only an internal error is not a transaction result.
    let (result, changes, contract_events) = match invoked {
        Ok(output) => match output.encoded_invoke_result {
            Ok(value) => {
                let changes = output
                    .ledger_changes
                    .into_iter()
                    .map(|change| Change {
                        read_only: change.read_only,
                        rent: rent_change(&change),
                        new_value: change.encoded_new_value,
                    })
                    .collect();
                (Ok(value), changes, output.encoded_contract_events)
            }
            Err(error) => (Err(failure(&error)?), Vec::new(), Vec::new()),
        },
        Err(error) => (Err(failure(&error)?), Vec::new(), Vec::new()),
    };

    Ok(Output {
        result,
        changes,
        contract_events,
        diagnostics: diagnostics
            .iter()
            .map(|event| event.to_xdr(Limits::none()).map_err(xdr))
            .collect::<Result<_, _>>()?,
        cpu_insns: budget
            .get_cpu_insns_consumed()
            .map_err(|e| Failure::Budget(e.to_string()))?,
        mem_bytes: budget
            .get_mem_bytes_consumed()
            .map_err(|e| Failure::Budget(e.to_string()))?,
    })
}

/// The `ScError` behind a failed run, or [`Failure::Start`] for the internal
/// errors stellar-core refuses to record as a result.
fn failure(error: &HostError) -> Result<Option<Vec<u8>>, Failure> {
    if error.error.is_code(ScErrorCode::InternalError)
        && !error.error.is_type(ScErrorType::Contract)
    {
        return Err(Failure::Start(error.to_string()));
    }
    match ScError::try_from(error) {
        Ok(error) => Ok(Some(error.to_xdr(Limits::none()).map_err(xdr)?)),
        Err(_) => Ok(None),
    }
}

/// The rent inputs of one change, when the host charges rent for it.
fn rent_change(
    change: &super::soroban_env_host::e2e_invoke::LedgerEntryChange,
) -> Option<RentChange> {
    let rent = extract_rent_changes(std::slice::from_ref(change)).pop()?;
    Some(RentChange {
        is_persistent: rent.is_persistent,
        is_code_entry: rent.is_code_entry,
        old_size_bytes: rent.old_size_bytes,
        new_size_bytes: rent.new_size_bytes,
        old_live_until_ledger: rent.old_live_until_ledger,
        new_live_until_ledger: rent.new_live_until_ledger,
    })
}

/// The fees stellar-core computes around one invocation, with the host's own
/// `fees` module: `TransactionFrame::computeSorobanResourceFee` and the
/// bridge's rent fee (checked 2026-11-11 against protocols 29 and 30).
pub fn fees(request: &FeeRequest) -> Fees {
    let settings = &request.settings;
    let (non_refundable, refundable) = compute_transaction_resource_fee(
        &TransactionResources {
            instructions: request.instructions,
            disk_read_entries: request.disk_read_entries,
            write_entries: request.write_entries,
            disk_read_bytes: request.disk_read_bytes,
            write_bytes: request.write_bytes,
            contract_events_size_bytes: request.contract_events_size_bytes,
            transaction_size_bytes: request.transaction_size_bytes,
        },
        &FeeConfiguration {
            fee_per_instruction_increment: settings.fee_per_instruction_increment,
            fee_per_disk_read_entry: settings.fee_per_disk_read_entry,
            fee_per_write_entry: settings.fee_per_write_entry,
            fee_per_disk_read_1kb: settings.fee_per_disk_read_1kb,
            fee_per_write_1kb: settings.fee_per_write_1kb,
            fee_per_historical_1kb: settings.fee_per_historical_1kb,
            fee_per_contract_event_1kb: settings.fee_per_contract_event_1kb,
            fee_per_transaction_size_1kb: settings.fee_per_transaction_size_1kb,
        },
    );
    let fee_per_rent_1kb = compute_rent_write_fee_per_1kb(
        settings.average_state_size_bytes,
        &RentWriteFeeConfiguration {
            state_target_size_bytes: settings.state_target_size_bytes,
            rent_fee_1kb_state_size_low: settings.rent_fee_1kb_state_size_low,
            rent_fee_1kb_state_size_high: settings.rent_fee_1kb_state_size_high,
            state_size_rent_fee_growth_factor: settings.state_size_rent_fee_growth_factor,
        },
    );
    let changes: Vec<LedgerEntryRentChange> = request
        .rent_changes
        .iter()
        .map(|change| LedgerEntryRentChange {
            is_persistent: change.is_persistent,
            is_code_entry: change.is_code_entry,
            old_size_bytes: change.old_size_bytes,
            new_size_bytes: change.new_size_bytes,
            old_live_until_ledger: change.old_live_until_ledger,
            new_live_until_ledger: change.new_live_until_ledger,
        })
        .collect();
    let rent = compute_rent_fee(
        &changes,
        &RentFeeConfiguration {
            fee_per_write_1kb: settings.fee_per_write_1kb,
            fee_per_rent_1kb,
            fee_per_write_entry: settings.fee_per_write_entry,
            persistent_rent_rate_denominator: settings.persistent_rent_rate_denominator,
            temporary_rent_rate_denominator: settings.temporary_rent_rate_denominator,
        },
        request.ledger_sequence,
    );
    Fees {
        non_refundable,
        refundable,
        rent,
    }
}

/// The size the host charges rent on: the encoded entry, plus the parsed
/// module's memory for contract code.
fn rent_size(budget: &Budget, entry_xdr: &[u8]) -> Option<u32> {
    let entry = LedgerEntry::from_xdr(entry_xdr, Limits::none()).ok()?;
    let size = u32::try_from(entry_xdr.len()).ok()?;
    entry_size_for_rent(budget, &entry, size).ok()
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
            contract.wasm,
            cost_inputs,
        ) {
            log::debug!(target: "replay", "module {} not cached: {e}", hex::encode(contract.hash));
        }
    }
    Ok(cache)
}
