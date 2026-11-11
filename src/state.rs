//! reconstruct: rebuild the ledger as it looked when the transaction ran.
//!
//! The RPC only serves *current* state, so this stage has two jobs: gather
//! every ledger entry the footprint touches, and decide how much each one can
//! be trusted as a stand-in for what the host saw at the transaction's ledger.
//!
//! # Value drift
//!
//! An entry whose `lastModifiedLedgerSeq` is at or after the transaction's
//! ledger may no longer hold the value the host saw. It is replayed as-is and
//! reported as drifted.
//!
//! # Liveness
//!
//! Contract data and code carry a TTL. An entry is live while
//! `ledger <= liveUntilLedgerSeq`; past that a temporary entry is gone and a
//! persistent one is archived until restored. What the host receives for each
//! footprint slot follows stellar-core's `InvokeHostFunctionOpFrame`:
//!
//! | entry at the transaction's ledger                | host receives            |
//! |--------------------------------------------------|--------------------------|
//! | classic entry (account, trustline)               | the entry, no TTL        |
//! | live (`liveUntil >= ledger`)                     | the entry and its TTL    |
//! | temporary, expired                               | nothing                  |
//! | persistent, archived, listed for auto-restore    | the entry, restored TTL  |
//! | persistent, archived, not listed                 | *(host is not invoked)*  |
//! | not on the ledger                                | nothing                  |
//!
//! An archived entry that is not listed for restoration makes stellar-core
//! fail the operation with `ENTRY_ARCHIVED` before the host runs; `replay`
//! mirrors that. A restored entry gets `ledger + minPersistentTTL - 1`, the
//! TTL core assigns on restoration.
//!
//! # What the RPC can and cannot tell us (checked against testnet, 2026-11-10)
//!
//! - `getLedgerEntries` rejects `LedgerKey::Ttl` keys ("ledger ttl entries
//!   cannot be queried directly"). The TTL is only available as the
//!   `liveUntilLedgerSeq` field of the data/code entry itself, with no
//!   modification sequence of its own. A durable entry returned without that
//!   field is an error ([`RewindError::MissingTtl`]); there is no fallback.
//! - An archived persistent entry (evicted to the hot archive) is still
//!   returned, body included, with `liveUntilLedgerSeq: 0`. The real TTL is
//!   lost, so `0` only says "archived *now*".
//!
//! # TTL drift
//!
//! A TTL extension or a restoration after the transaction changes
//! `liveUntilLedgerSeq` without touching the entry's own
//! `lastModifiedLedgerSeq`, so it is invisible to the value-drift check. Two
//! cases can still be proven from the data and are reported as TTL drift:
//!
//! - `liveUntil > ledger + maxEntryTTL - 1`: no TTL set at or before the
//!   transaction's ledger can reach that far, so it was extended afterwards
//!   and the entry's liveness when the transaction ran is unproven.
//! - `liveUntil == 0` on an entry the transaction did not list for
//!   restoration, while the network did *not* record `ENTRY_ARCHIVED`: core
//!   would have refused an archived entry, so it was live then and has been
//!   archived since. It is replayed as live.

use std::collections::HashSet;

use sha2::{Digest, Sha256};
use stellar_xdr::{
    ConfigSettingEntry, ConfigSettingId, ContractCostParams, ContractDataDurability,
    InvokeHostFunctionResult, LedgerEntry, LedgerEntryData, LedgerHeaderHistoryEntry, LedgerKey,
    LedgerKeyConfigSetting,
};

use crate::codec;
use crate::display;
use crate::error::{Result, RewindError};
use crate::host::{Entry, LedgerInfo};
use crate::rpc::{RawLedgerEntry, RpcClient};
use crate::tx::DecodedTx;

/// The ledger the transaction was applied in, as the host needs to see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerContext {
    pub sequence: u32,
    pub protocol: u32,
    pub base_reserve: u32,
    pub close_time: u64,
    /// SHA-256 of the network passphrase.
    pub network_id: [u8; 32],
    /// Hash of the ledger's transaction set; the base of the Soroban PRNG seed.
    pub tx_set_hash: [u8; 32],
}

/// Read the header of the transaction's own ledger: it carries the protocol
/// version and base reserve the host needs, and its close time is the
/// timestamp the host sees.
pub async fn read_ledger(
    rpc: &RpcClient,
    sequence: u32,
    passphrase: &str,
) -> Result<LedgerContext> {
    let ledger = rpc
        .get_ledger(sequence)
        .await?
        .ok_or(RewindError::LedgerUnavailable { sequence })?;

    // The RPC hands back a `LedgerHeaderHistoryEntry` (hash + header), not a
    // bare `LedgerHeader`.
    let history: LedgerHeaderHistoryEntry = codec::decode(&ledger.header_xdr, "headerXdr")?;
    let header = history.header;
    let close_time =
        ledger
            .ledger_close_time
            .parse::<u64>()
            .map_err(|_| RewindError::MissingField {
                method: "getLedgers",
                field: "ledgerCloseTime (a decimal timestamp)",
            })?;

    Ok(LedgerContext {
        sequence,
        protocol: header.ledger_version,
        base_reserve: header.base_reserve,
        close_time,
        network_id: Sha256::digest(passphrase.as_bytes()).into(),
        tx_set_hash: header.scp_value.tx_set_hash.0,
    })
}

/// Network parameters the host needs that do not live in the transaction.
/// Every value comes from a `ConfigSetting` ledger entry; there are no
/// built-in defaults, because a replay priced with made-up parameters is not
/// comparable with the network's.
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub cpu_cost_params: ContractCostParams,
    pub mem_cost_params: ContractCostParams,
    pub tx_memory_limit: u32,
    pub tx_max_contract_events_size_bytes: u32,
    pub max_entry_ttl: u32,
    pub min_temporary_ttl: u32,
    pub min_persistent_ttl: u32,
    /// Settings modified at or after the transaction's ledger: the network was
    /// reconfigured since, so the replay may be priced differently.
    pub drifted: Vec<&'static str>,
}

/// The settings the replay depends on, in the order they are requested.
const CONFIG_SETTINGS: [ConfigSettingId; 5] = [
    ConfigSettingId::ContractCostParamsCpuInstructions,
    ConfigSettingId::ContractCostParamsMemoryBytes,
    ConfigSettingId::ContractComputeV0,
    ConfigSettingId::ContractEventsV0,
    ConfigSettingId::StateArchival,
];

fn build_config(entries: &[Option<RawLedgerEntry>], tx_ledger: u32) -> Result<NetworkConfig> {
    let mut cpu_cost_params = None;
    let mut mem_cost_params = None;
    let mut compute = None;
    let mut events = None;
    let mut archival = None;
    let mut drifted = Vec::new();

    for (id, raw) in CONFIG_SETTINGS.iter().zip(entries) {
        let setting = id.name();
        let raw = raw
            .as_ref()
            .ok_or(RewindError::ConfigSettingMissing { setting })?;
        let malformed = |detail: String| RewindError::ConfigSettingMalformed { setting, detail };

        let entry = codec::assemble_entry(raw).map_err(|e| malformed(e.to_string()))?;
        let LedgerEntryData::ConfigSetting(value) = entry.data else {
            return Err(malformed("the entry is not a config setting".to_string()));
        };
        if value.discriminant() != *id {
            return Err(malformed(format!("the entry holds `{}`", value.name())));
        }
        match value {
            ConfigSettingEntry::ContractCostParamsCpuInstructions(p) => cpu_cost_params = Some(p),
            ConfigSettingEntry::ContractCostParamsMemoryBytes(p) => mem_cost_params = Some(p),
            ConfigSettingEntry::ContractComputeV0(c) => compute = Some(c),
            ConfigSettingEntry::ContractEventsV0(e) => events = Some(e),
            ConfigSettingEntry::StateArchival(s) => archival = Some(s),
            other => return Err(malformed(format!("unexpected setting `{}`", other.name()))),
        }
        if raw.last_modified_ledger_seq.is_some_and(|m| m >= tx_ledger) {
            drifted.push(setting);
        }
    }

    let missing = |id: ConfigSettingId| RewindError::ConfigSettingMissing { setting: id.name() };
    let compute = compute.ok_or_else(|| missing(ConfigSettingId::ContractComputeV0))?;
    let events = events.ok_or_else(|| missing(ConfigSettingId::ContractEventsV0))?;
    let archival = archival.ok_or_else(|| missing(ConfigSettingId::StateArchival))?;
    Ok(NetworkConfig {
        cpu_cost_params: cpu_cost_params
            .ok_or_else(|| missing(ConfigSettingId::ContractCostParamsCpuInstructions))?,
        mem_cost_params: mem_cost_params
            .ok_or_else(|| missing(ConfigSettingId::ContractCostParamsMemoryBytes))?,
        tx_memory_limit: compute.tx_memory_limit,
        tx_max_contract_events_size_bytes: events.tx_max_contract_events_size_bytes,
        max_entry_ttl: archival.max_entry_ttl,
        min_temporary_ttl: archival.min_temporary_ttl,
        min_persistent_ttl: archival.min_persistent_ttl,
        drifted,
    })
}

/// Whether a footprint entry existed, as far as the host is concerned, at the
/// transaction's ledger. See the module docs for the rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// A classic entry; it has no TTL.
    Untracked,
    Live {
        live_until: u32,
    },
    /// Archived when the transaction ran and listed for automatic restoration.
    Restored {
        live_until: u32,
    },
    /// Persistent, past its TTL, and not listed for restoration.
    Archived,
    /// Temporary and past its TTL: to the host it does not exist.
    Expired,
    /// The RPC has no such entry.
    Absent,
}

/// Why an entry's `liveUntilLedgerSeq` is known to have changed since the
/// transaction ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlDrift {
    /// The TTL reaches further than any extension made by then could.
    ExtendedSince,
    /// Archived now, but demonstrably live when the transaction ran.
    ArchivedSince,
}

/// The facts liveness is decided against.
#[derive(Debug, Clone, Copy)]
pub struct LivenessRule {
    pub tx_ledger: u32,
    pub max_entry_ttl: u32,
    pub min_persistent_ttl: u32,
    /// The network recorded `ENTRY_ARCHIVED` for this transaction.
    pub onchain_entry_archived: bool,
}

impl LivenessRule {
    /// Classify one durable entry. `restorable` says whether the transaction
    /// listed the entry for automatic restoration.
    pub fn classify(
        &self,
        durability: ContractDataDurability,
        live_until: u32,
        restorable: bool,
    ) -> (Liveness, Option<TtlDrift>) {
        if live_until >= self.tx_ledger {
            let furthest_possible = self
                .tx_ledger
                .saturating_add(self.max_entry_ttl)
                .saturating_sub(1);
            let drift = (live_until > furthest_possible).then_some(TtlDrift::ExtendedSince);
            return (Liveness::Live { live_until }, drift);
        }
        match durability {
            ContractDataDurability::Temporary => (Liveness::Expired, None),
            ContractDataDurability::Persistent if restorable => (
                Liveness::Restored {
                    live_until: self
                        .tx_ledger
                        .saturating_add(self.min_persistent_ttl)
                        .saturating_sub(1),
                },
                None,
            ),
            // `0` means "archived now"; the TTL it had back then is lost. Core
            // refuses archived entries, so unless it did refuse, it was live.
            ContractDataDurability::Persistent
                if live_until == 0 && !self.onchain_entry_archived =>
            {
                (
                    Liveness::Live {
                        live_until: self.tx_ledger,
                    },
                    Some(TtlDrift::ArchivedSince),
                )
            }
            ContractDataDurability::Persistent => (Liveness::Archived, None),
        }
    }
}

/// The durability of a key's entry; `None` for classic entries, which have
/// no TTL.
fn durability(key: &LedgerKey) -> Option<ContractDataDurability> {
    match key {
        LedgerKey::ContractData(data) => Some(data.durability),
        LedgerKey::ContractCode(_) => Some(ContractDataDurability::Persistent),
        _ => None,
    }
}

/// One footprint slot, plus what we learned about the entry living in it.
#[derive(Debug, Clone)]
pub struct FootprintSlot {
    pub key: LedgerKey,
    pub key_xdr: Vec<u8>,
    pub entry: Option<LedgerEntry>,
    pub last_modified: Option<u32>,
    pub liveness: Liveness,
    pub ttl_drift: Option<TtlDrift>,
}

impl FootprintSlot {
    fn new(
        key: LedgerKey,
        key_xdr: Vec<u8>,
        raw: Option<&RawLedgerEntry>,
        rule: &LivenessRule,
        restorable: bool,
    ) -> Result<Self> {
        let Some(raw) = raw else {
            return Ok(Self {
                key,
                key_xdr,
                entry: None,
                last_modified: None,
                liveness: Liveness::Absent,
                ttl_drift: None,
            });
        };
        let entry = codec::assemble_entry(raw)?;
        let (liveness, ttl_drift) = match durability(&key) {
            None => (Liveness::Untracked, None),
            Some(durability) => {
                let live_until =
                    raw.live_until_ledger_seq
                        .ok_or_else(|| RewindError::MissingTtl {
                            entry: display::ledger_key(&key),
                        })?;
                rule.classify(durability, live_until, restorable)
            }
        };
        Ok(Self {
            key,
            key_xdr,
            entry: Some(entry),
            last_modified: raw.last_modified_ledger_seq,
            liveness,
            ttl_drift,
        })
    }

    /// The entry was written at or after the transaction's own ledger, so the
    /// value we fetched may not be the value the host saw.
    pub fn value_drifted(&self, tx_ledger: u32) -> bool {
        self.last_modified.is_some_and(|m| m >= tx_ledger)
    }

    /// The host is handed no entry for this slot.
    pub fn missing(&self) -> bool {
        matches!(self.liveness, Liveness::Absent | Liveness::Expired)
    }

    pub fn archived(&self) -> bool {
        self.liveness == Liveness::Archived
    }

    /// The slot as the host should see it; `None` where it sees no entry.
    fn encoded(&self) -> Result<Option<Entry>> {
        let live_until = match self.liveness {
            Liveness::Absent | Liveness::Expired | Liveness::Archived => return Ok(None),
            Liveness::Untracked => None,
            Liveness::Live { live_until } | Liveness::Restored { live_until } => Some(live_until),
        };
        let Some(entry) = &self.entry else {
            return Ok(None);
        };
        Ok(Some(Entry {
            xdr: codec::encode(entry, "a footprint entry")?,
            live_until,
        }))
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
    /// Encode the slots exactly as `invoke_host_function` wants them: one item
    /// per footprint key, `None` where the host should see no entry.
    pub fn encoded_entries(&self) -> Result<Vec<Option<Entry>>> {
        self.slots.iter().map(FootprintSlot::encoded).collect()
    }

    /// Look an entry up among the footprint slots by its encoded key.
    pub fn slot(&self, key_xdr: &[u8]) -> Option<&FootprintSlot> {
        self.slots.iter().find(|slot| slot.key_xdr == key_xdr)
    }
}

pub async fn reconstruct(
    rpc: &RpcClient,
    tx: &DecodedTx,
    ledger: &LedgerContext,
) -> Result<Reconstructed> {
    let footprint = tx.footprint_keys();
    let key_xdr: Vec<Vec<u8>> = footprint
        .iter()
        .map(|key| codec::encode(key, "a footprint key"))
        .collect::<Result<_>>()?;

    // Config settings are not part of the footprint but the host needs them to
    // price execution the way the network did. One request fetches both.
    let mut keys: Vec<String> = key_xdr
        .iter()
        .map(|bytes| codec::b64_encode(bytes))
        .collect();
    for id in CONFIG_SETTINGS {
        let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: id,
        });
        keys.push(codec::b64_encode(&codec::encode(
            &key,
            "a config setting key",
        )?));
    }

    let mut entries = rpc.get_ledger_entries(&keys).await?;
    let config_entries = entries.split_off(footprint.len());
    let config = build_config(&config_entries, ledger.sequence)?;

    let rule = LivenessRule {
        tx_ledger: ledger.sequence,
        max_entry_ttl: config.max_entry_ttl,
        min_persistent_ttl: config.min_persistent_ttl,
        onchain_entry_archived: tx.onchain_result == InvokeHostFunctionResult::EntryArchived,
    };
    // Restore indices point into the read-write half of the footprint.
    let read_only = tx.resources.footprint.read_only.len();
    let restorable: HashSet<usize> = tx
        .restored_rw_indices
        .iter()
        .map(|&i| read_only + i as usize)
        .collect();

    let slots = footprint
        .into_iter()
        .zip(key_xdr)
        .zip(&entries)
        .enumerate()
        .map(|(i, ((key, key_xdr), raw))| {
            FootprintSlot::new(key, key_xdr, raw.as_ref(), &rule, restorable.contains(&i))
        })
        .collect::<Result<Vec<_>>>()?;

    let ledger_info = LedgerInfo {
        protocol_version: ledger.protocol,
        sequence_number: ledger.sequence,
        timestamp: ledger.close_time,
        network_id: ledger.network_id,
        base_reserve: ledger.base_reserve,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        account_entry, code_entry, config_entries, config_entry, data_entry, raw_entry,
    };
    use ContractDataDurability::{Persistent, Temporary};

    const TX_LEDGER: u32 = 1_000;

    fn rule() -> LivenessRule {
        LivenessRule {
            tx_ledger: TX_LEDGER,
            max_entry_ttl: 500,
            min_persistent_ttl: 100,
            onchain_entry_archived: false,
        }
    }

    #[test]
    fn live_entries_keep_their_ttl() {
        for durability in [Persistent, Temporary] {
            // Live through the transaction's own ledger, inclusive.
            assert_eq!(
                rule().classify(durability, TX_LEDGER, false),
                (
                    Liveness::Live {
                        live_until: TX_LEDGER
                    },
                    None
                )
            );
            assert_eq!(
                rule().classify(durability, TX_LEDGER + 499, false),
                (
                    Liveness::Live {
                        live_until: TX_LEDGER + 499
                    },
                    None
                )
            );
        }
    }

    #[test]
    fn a_ttl_beyond_the_maximum_was_extended_later() {
        assert_eq!(
            rule().classify(Persistent, TX_LEDGER + 500, false),
            (
                Liveness::Live {
                    live_until: TX_LEDGER + 500
                },
                Some(TtlDrift::ExtendedSince)
            )
        );
    }

    #[test]
    fn expired_temporary_entries_do_not_exist() {
        assert_eq!(
            rule().classify(Temporary, TX_LEDGER - 1, false),
            (Liveness::Expired, None)
        );
        assert_eq!(
            rule().classify(Temporary, 0, false),
            (Liveness::Expired, None)
        );
        // Being listed for restoration does not bring a temporary entry back.
        assert_eq!(
            rule().classify(Temporary, TX_LEDGER - 1, true),
            (Liveness::Expired, None)
        );
    }

    #[test]
    fn archived_persistent_entries_need_restoration() {
        assert_eq!(
            rule().classify(Persistent, TX_LEDGER - 1, false),
            (Liveness::Archived, None)
        );
        assert_eq!(
            rule().classify(Persistent, TX_LEDGER - 1, true),
            (
                Liveness::Restored {
                    live_until: TX_LEDGER + 99
                },
                None
            )
        );
        assert_eq!(
            rule().classify(Persistent, 0, true),
            (
                Liveness::Restored {
                    live_until: TX_LEDGER + 99
                },
                None
            )
        );
    }

    #[test]
    fn archived_now_is_judged_by_what_the_network_recorded() {
        // The network ran the host, so the entry was live back then.
        assert_eq!(
            rule().classify(Persistent, 0, false),
            (
                Liveness::Live {
                    live_until: TX_LEDGER
                },
                Some(TtlDrift::ArchivedSince)
            )
        );
        // The network refused with ENTRY_ARCHIVED: it was archived then too.
        let refused = LivenessRule {
            onchain_entry_archived: true,
            ..rule()
        };
        assert_eq!(
            refused.classify(Persistent, 0, false),
            (Liveness::Archived, None)
        );
    }

    #[test]
    fn slots_follow_the_liveness_table() {
        let slot = |entry: &LedgerEntry, live_until: Option<u32>, restorable: bool| {
            let raw = raw_entry(entry, live_until);
            let key = crate::test_support::key_of(entry);
            FootprintSlot::new(key, vec![1], Some(&raw), &rule(), restorable)
        };

        // Classic entry: handed over without TTL metadata.
        let classic = slot(&account_entry([1; 32], 10), None, false).unwrap();
        assert_eq!(classic.liveness, Liveness::Untracked);
        assert_eq!(classic.encoded().unwrap().unwrap().live_until, None);

        // Live durable entry: entry plus TTL.
        let live = slot(
            &data_entry([2; 32], Persistent, 10),
            Some(TX_LEDGER + 5),
            false,
        )
        .unwrap();
        assert_eq!(
            live.encoded().unwrap().unwrap().live_until,
            Some(TX_LEDGER + 5)
        );
        assert!(!live.missing() && !live.archived());

        // Expired temporary entry: nothing.
        let expired = slot(
            &data_entry([2; 32], Temporary, 10),
            Some(TX_LEDGER - 1),
            false,
        )
        .unwrap();
        assert!(expired.missing());
        assert_eq!(expired.encoded().unwrap(), None);

        // Archived code entry: flagged, and never handed to the host.
        let archived = slot(&code_entry(b"wasm", 10), Some(TX_LEDGER - 1), false).unwrap();
        assert!(archived.archived());
        assert_eq!(archived.encoded().unwrap(), None);

        // Restorable archived entry: handed over with the restored TTL.
        let restored = slot(&code_entry(b"wasm", 10), Some(TX_LEDGER - 1), true).unwrap();
        assert_eq!(
            restored.encoded().unwrap().unwrap().live_until,
            Some(TX_LEDGER + 99)
        );

        // Absent entry.
        let key = crate::test_support::key_of(&account_entry([1; 32], 10));
        let absent = FootprintSlot::new(key, vec![1], None, &rule(), false).unwrap();
        assert_eq!(absent.liveness, Liveness::Absent);
        assert!(absent.missing());
    }

    #[test]
    fn a_durable_entry_without_ttl_is_an_error_that_names_it() {
        let entry = data_entry([2; 32], Persistent, 10);
        let raw = raw_entry(&entry, None);
        let key = crate::test_support::key_of(&entry);
        let err = FootprintSlot::new(key, vec![1], Some(&raw), &rule(), false).unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, RewindError::MissingTtl { .. }));
        assert!(
            text.contains("contract data C…") && text.contains("liveUntilLedgerSeq"),
            "{text}"
        );
    }

    #[test]
    fn value_drift_is_at_or_after_the_transaction_ledger() {
        let drifted = |last_modified| {
            let entry = account_entry([1; 32], last_modified);
            let raw = raw_entry(&entry, None);
            let key = crate::test_support::key_of(&entry);
            FootprintSlot::new(key, vec![1], Some(&raw), &rule(), false)
                .unwrap()
                .value_drifted(TX_LEDGER)
        };
        assert!(!drifted(TX_LEDGER - 1));
        assert!(drifted(TX_LEDGER));
        assert!(drifted(TX_LEDGER + 1));
    }

    #[test]
    fn builds_config_from_complete_settings() {
        let config = build_config(&config_entries(TX_LEDGER - 1), TX_LEDGER).unwrap();
        assert_eq!(config.tx_memory_limit, 41_943_040);
        assert_eq!(config.tx_max_contract_events_size_bytes, 16_384);
        assert_eq!(config.max_entry_ttl, 3_110_400);
        assert_eq!(config.min_temporary_ttl, 17_280);
        assert_eq!(config.min_persistent_ttl, 2_073_600);
        assert!(config.drifted.is_empty());
    }

    #[test]
    fn config_modified_since_the_transaction_is_drift() {
        let config = build_config(&config_entries(TX_LEDGER), TX_LEDGER).unwrap();
        assert_eq!(config.drifted.len(), CONFIG_SETTINGS.len());
        assert!(config.drifted.contains(&"StateArchival"));
    }

    #[test]
    fn missing_config_is_a_hard_error() {
        let mut entries = config_entries(1);
        entries[2] = None;
        let err = build_config(&entries, TX_LEDGER).unwrap_err();
        assert!(
            matches!(
                err,
                RewindError::ConfigSettingMissing {
                    setting: "ContractComputeV0"
                }
            ),
            "{err}"
        );
        // A short response is missing settings too.
        let err = build_config(&config_entries(1)[..4], TX_LEDGER).unwrap_err();
        assert!(
            matches!(
                err,
                RewindError::ConfigSettingMissing {
                    setting: "StateArchival"
                }
            ),
            "{err}"
        );
        assert!(matches!(
            build_config(&[], TX_LEDGER),
            Err(RewindError::ConfigSettingMissing { .. })
        ));
    }

    #[test]
    fn malformed_config_is_a_hard_error() {
        // Undecodable body.
        let mut entries = config_entries(1);
        if let Some(raw) = entries[0].as_mut() {
            raw.xdr = "AAAA".into();
        }
        let err = build_config(&entries, TX_LEDGER).unwrap_err();
        assert!(
            matches!(
                err,
                RewindError::ConfigSettingMalformed {
                    setting: "ContractCostParamsCpuInstructions",
                    ..
                }
            ),
            "{err}"
        );

        // Not a config setting at all.
        let mut entries = config_entries(1);
        entries[4] = Some(raw_entry(&account_entry([1; 32], 1), None));
        let err = build_config(&entries, TX_LEDGER).unwrap_err().to_string();
        assert!(
            err.contains("`StateArchival`") && err.contains("not a config setting"),
            "{err}"
        );

        // The right kind of entry in the wrong slot.
        let mut entries = config_entries(1);
        entries[3] = Some(config_entry(ConfigSettingId::StateArchival, 1));
        let err = build_config(&entries, TX_LEDGER).unwrap_err().to_string();
        assert!(
            err.contains("`ContractEventsV0`") && err.contains("holds `StateArchival`"),
            "{err}"
        );
    }
}
