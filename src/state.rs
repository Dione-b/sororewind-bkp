//! reconstruct: rebuild the ledger as it looked when the transaction ran.
//!
//! Two jobs: gather every ledger entry the footprint touches, and decide how
//! much to trust each one. An entry modified at or after the transaction's own
//! ledger may no longer hold the value the host saw, so it is marked *drifted*
//! and the report says so rather than silently pretending the replay is exact.

use sha2::{Digest, Sha256};
use soroban_env_host::{
    budget::Budget,
    e2e_invoke::{entry_size_for_rent, TtlLedgerEntryMeta},
    xdr::{Limits, WriteXdr},
    LedgerInfo,
};
use stellar_xdr::{
    ConfigSettingEntry, ConfigSettingId, ContractCostParams, LedgerEntry, LedgerEntryData,
    LedgerHeader, LedgerHeaderHistoryEntry, LedgerKey, ReadXdr, StateArchivalSettings,
};

use crate::error::{Result, RewindError};
use crate::rpc::{self, RpcClient};

/// Network parameters the host needs that do not live in the transaction.
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub cpu_cost_params: ContractCostParams,
    pub mem_cost_params: ContractCostParams,
    pub tx_memory_limit: u32,
    pub max_entry_ttl: u32,
    pub min_temporary_ttl: u32,
    pub min_persistent_ttl: u32,
    /// Whether every value above came from a real `ConfigSetting` entry, or we
    /// had to fall back to protocol defaults. Printed under `-v` because a
    /// fallback can shift budget accounting.
    pub from_ledger: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        // Protocol-30 defaults for testnet/mainnet, used only when the RPC
        // declines to serve ConfigSetting keys.
        Self {
            cpu_cost_params: ContractCostParams::default(),
            mem_cost_params: ContractCostParams::default(),
            tx_memory_limit: 40_000_000,
            max_entry_ttl: 6_312_000,
            min_temporary_ttl: 16,
            min_persistent_ttl: 4_096,
            from_ledger: false,
        }
    }
}

/// One footprint slot, plus what we learned about the entry living in it.
#[derive(Debug, Clone)]
pub struct FootprintSlot {
    pub key: LedgerKey,
    pub key_xdr: Vec<u8>,
    pub entry: Option<LedgerEntry>,
    pub last_modified: Option<u32>,
    pub live_until: Option<u32>,
}

impl FootprintSlot {
    /// The entry was written at or after the transaction's own ledger, so the
    /// value we fetched may not be the value the host saw.
    pub fn drifted(&self, tx_ledger: u32) -> bool {
        self.last_modified.is_some_and(|m| m >= tx_ledger)
    }

    pub fn missing(&self) -> bool {
        self.entry.is_none()
    }
}

#[derive(Debug)]
pub struct Reconstructed {
    pub ledger_info: LedgerInfo,
    pub config: NetworkConfig,
    /// In footprint order: read-only keys first, then read-write.
    pub slots: Vec<FootprintSlot>,
}

impl Reconstructed {
    pub fn drifted(&self, tx_ledger: u32) -> Vec<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.drifted(tx_ledger))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn missing(&self) -> Vec<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.missing())
            .map(|(i, _)| i)
            .collect()
    }

    /// Encode the slots exactly as `invoke_host_function` wants them: one item
    /// per footprint key, `None` where no live entry exists.
    pub fn encoded_entries(&self) -> Result<Vec<(Option<Vec<u8>>, Option<TtlLedgerEntryMeta>)>> {
        let mut out = Vec::with_capacity(self.slots.len());
        for slot in &self.slots {
            match &slot.entry {
                None => out.push((None, None)),
                Some(entry) => {
                    let bytes = entry
                        .to_xdr(Limits::none())
                        .map_err(|e| RewindError::State(format!("encoding ledger entry: {e}")))?;
                    let ttl = match slot.live_until {
                        Some(live_until) => Some(TtlLedgerEntryMeta {
                            live_until_ledger: live_until,
                            entry_size_for_rent: entry_size_for_rent(
                                &Budget::default(),
                                entry,
                                bytes.len() as u32,
                            )
                            .map_err(|e| {
                                RewindError::State(format!("computing entry size: {e}"))
                            })?,
                        }),
                        None => None,
                    };
                    out.push((Some(bytes), ttl));
                }
            }
        }
        Ok(out)
    }
}

pub async fn reconstruct(
    rpc: &RpcClient,
    footprint: &[LedgerKey],
    tx_ledger: u32,
    network_passphrase: &str,
    protocol_version: u32,
    base_reserve: u32,
    ledger_close_time: u64,
) -> Result<Reconstructed> {
    let key_xdr: Vec<Vec<u8>> = footprint
        .iter()
        .map(|k| {
            k.to_xdr(Limits::none())
                .map_err(|e| RewindError::State(format!("encoding footprint key: {e}")))
        })
        .collect::<Result<_>>()?;
    let key_b64: Vec<String> = key_xdr.iter().map(|b| rpc::b64_encode(b)).collect();

    // Config settings are not part of the footprint but the host needs them to
    // price execution the way the network did.
    let config_keys: Vec<String> = CONFIG_SETTINGS
        .iter()
        .map(|id| {
            let key = LedgerKey::ConfigSetting(stellar_xdr::LedgerKeyConfigSetting {
                config_setting_id: *id,
            });
            let bytes = key
                .to_xdr(Limits::none())
                .expect("config setting keys always encode");
            rpc::b64_encode(&bytes)
        })
        .collect();

    let mut all_keys = key_b64.clone();
    all_keys.extend(config_keys.iter().cloned());

    let returned = rpc.get_ledger_entries(&all_keys).await?;
    let mut ordered = rpc::order_entries(&all_keys, returned);

    let entry_keys: Vec<Option<rpc::RawLedgerEntry>> = ordered.drain(..key_b64.len()).collect();
    let config_entries: Vec<Option<rpc::RawLedgerEntry>> = ordered;

    let config = build_config(&config_entries);

    let mut slots = Vec::with_capacity(footprint.len());
    for (key, raw) in footprint.iter().zip(entry_keys) {
        let key_bytes = key_xdr.get(slots.len()).cloned().unwrap_or_default();
        match raw {
            Some(raw) => {
                let entry = rpc::assemble_entry(&raw)?;
                slots.push(FootprintSlot {
                    key: key.clone(),
                    key_xdr: key_bytes,
                    entry: Some(entry),
                    last_modified: raw.last_modified_ledger_seq,
                    live_until: raw.live_until_ledger_seq,
                });
            }
            None => slots.push(FootprintSlot {
                key: key.clone(),
                key_xdr: key_bytes,
                entry: None,
                last_modified: None,
                live_until: None,
            }),
        }
    }

    // Durable entries must carry TTL metadata or the host treats the whole
    // replay as an internal error. If the RPC did not tell us, go and ask the
    // TTL ledger key directly.
    let missing_ttl: Vec<usize> = slots
        .iter()
        .enumerate()
        .filter(|(_, s)| s.entry.is_some() && s.live_until.is_none() && has_durability(&s.key))
        .map(|(i, _)| i)
        .collect();

    if !missing_ttl.is_empty() {
        let ttl_keys: Vec<String> = missing_ttl
            .iter()
            .map(|&i| {
                let h = Sha256::digest(&slots[i].key_xdr);
                let key = LedgerKey::Ttl(stellar_xdr::LedgerKeyTtl {
                    key_hash: stellar_xdr::Hash(h.into()),
                });
                let bytes = key.to_xdr(Limits::none()).expect("ttl keys always encode");
                rpc::b64_encode(&bytes)
            })
            .collect();

        let returned = rpc.get_ledger_entries(&ttl_keys).await?;
        let ordered = rpc::order_entries(&ttl_keys, returned);
        for (idx, raw) in missing_ttl.iter().zip(ordered) {
            if let Some(raw) = raw {
                slots[*idx].live_until = raw.live_until_ledger_seq;
            }
        }
    }

    let network_id = {
        let mut h = Sha256::new();
        h.update(network_passphrase.as_bytes());
        h.finalize().into()
    };

    let ledger_info = LedgerInfo {
        protocol_version,
        sequence_number: tx_ledger,
        timestamp: ledger_close_time,
        network_id,
        base_reserve,
        min_temp_entry_ttl: config.min_temporary_ttl,
        min_persistent_entry_ttl: config.min_persistent_ttl,
        max_entry_ttl: config.max_entry_ttl,
    };

    Ok(Reconstructed {
        ledger_info,
        config,
        slots,
    })
}

const CONFIG_SETTINGS: [ConfigSettingId; 4] = [
    ConfigSettingId::ContractCostParamsCpuInstructions,
    ConfigSettingId::ContractCostParamsMemoryBytes,
    ConfigSettingId::ContractComputeV0,
    ConfigSettingId::StateArchival,
];

fn build_config(entries: &[Option<rpc::RawLedgerEntry>]) -> NetworkConfig {
    let mut config = NetworkConfig::default();
    let mut all_found = true;

    for raw in entries {
        let Some(raw) = raw else {
            all_found = false;
            continue;
        };
        let Ok(entry) = rpc::assemble_entry(raw) else {
            all_found = false;
            continue;
        };
        let LedgerEntryData::ConfigSetting(setting) = entry.data else {
            all_found = false;
            continue;
        };
        match setting {
            ConfigSettingEntry::ContractCostParamsCpuInstructions(p) => config.cpu_cost_params = p,
            ConfigSettingEntry::ContractCostParamsMemoryBytes(p) => config.mem_cost_params = p,
            ConfigSettingEntry::ContractComputeV0(c) => {
                config.tx_memory_limit = c.tx_memory_limit;
            }
            ConfigSettingEntry::StateArchival(s) => {
                let StateArchivalSettings {
                    max_entry_ttl,
                    min_temporary_ttl,
                    min_persistent_ttl,
                    ..
                } = s;
                config.max_entry_ttl = max_entry_ttl;
                config.min_temporary_ttl = min_temporary_ttl;
                config.min_persistent_ttl = min_persistent_ttl;
            }
            _ => all_found = false,
        }
    }

    config.from_ledger = all_found;
    config
}

fn has_durability(key: &LedgerKey) -> bool {
    matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_))
}

/// Read the ledger header for the transaction's own ledger: it carries the
/// protocol version and base reserve the host needs, and its close time is the
/// timestamp the host sees.
pub async fn read_ledger_header(rpc: &RpcClient, sequence: u32) -> Result<(LedgerHeader, u64)> {
    let resp = rpc.get_ledgers(sequence, 1).await?;
    let ledger = resp
        .ledgers
        .into_iter()
        .find(|l| l.sequence == sequence)
        .ok_or_else(|| {
            RewindError::State(format!(
                "the RPC has no ledger {sequence} in its retention window; \
                 it can no longer tell us the state that transaction ran against"
            ))
        })?;

    let header_bytes = rpc::b64_decode(&ledger.header_xdr, "headerXdr")?;
    // The RPC hands back a `LedgerHeaderHistoryEntry` (hash + header), not a
    // bare `LedgerHeader`.
    let history = LedgerHeaderHistoryEntry::from_xdr(&header_bytes, Limits::none())
        .map_err(|e| RewindError::State(format!("decoding ledger header: {e}")))?;
    let header = history.header;

    let close_time = ledger
        .ledger_close_time
        .parse::<u64>()
        .map_err(|e| RewindError::State(format!("bad ledgerCloseTime: {e}")))?;

    Ok((header, close_time))
}
