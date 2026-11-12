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
//! TTL core assigns on restoration. Being listed does not restore an entry
//! that is live: core only restores (and only tells the host about) the
//! listed entries it actually finds archived.
//!
//! # The ledger the transaction saw
//!
//! stellar-core applies a ledger's transactions first and its upgrades
//! afterwards, so the stored header of ledger N already carries the upgraded
//! protocol version and base reserve while N's transactions ran under those
//! of N-1. When the header's `scpValue.upgrades` changes either, it is read
//! from the previous header.
//!
//! # Rebuilding the past
//!
//! Entries that changed since the transaction are rebuilt from ledger
//! metadata where the RPC still has it; see [`crate::history`]. What cannot
//! be rebuilt is replayed as it is now and reported as drifted.
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
//!   restoration, while the network did *not* record `ENTRY_ARCHIVED` for it:
//!   core would have refused an archived entry, so it was live then and has
//!   been archived since. It is replayed as live. When the network did record
//!   `ENTRY_ARCHIVED`, its diagnostic names the entry it refused, and only
//!   that one is taken to have been archived; without diagnostics every such
//!   entry is, and the report says the liveness was inferred.
//! - an entry listed for restoration that is live now with a TTL a
//!   restoration at or after the transaction's ledger would have produced
//!   (`liveUntil >= ledger + minPersistentTTL - 1`): the transaction failed,
//!   so it restored nothing, and something else has restored the entry since.
//!   It is replayed as archived-and-restored. A smaller TTL can only have
//!   been set before the transaction, so that entry was live.
//!
//! An entry ledger metadata shows was restored after the transaction, before
//! any other change to it or its TTL, is judged like one archived now
//! (`liveUntil == 0`): it was archived by the time of the restoration, and
//! the TTL it had when the transaction ran is lost, so the current one, set
//! by the restoration, says nothing about it.

use std::collections::HashSet;

use sha2::{Digest, Sha256};
use stellar_xdr::{
    ConfigSettingEntry, ConfigSettingId, ContractCostParams, ContractDataDurability,
    DiagnosticEvent, InvokeHostFunctionResult, LedgerEntry, LedgerEntryData, LedgerHeader,
    LedgerHeaderHistoryEntry, LedgerKey, LedgerKeyConfigSetting, LedgerKeyContractCode,
    LedgerKeyContractData, LedgerUpgrade, Limits, ReadXdr, ScVal,
};

use crate::codec;
use crate::display;
use crate::error::{Result, RewindError};
use crate::history::{self, Past, PastValue, Rebuild};
use crate::host::{Entry, FeeSettings, LedgerInfo};
use crate::rpc::{RawLedgerEntry, RpcClient};
use crate::tx::DecodedTx;

/// The ledger the transaction was applied in, as the host needs to see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerContext {
    /// The ledger sequence number.
    pub sequence: u32,
    /// The protocol the transaction ran under: the previous header's when
    /// this ledger upgraded it.
    pub protocol: u32,
    /// The base reserve the transaction ran under, in stroops: the previous
    /// header's when this ledger upgraded it.
    pub base_reserve: u32,
    /// `scpValue.closeTime`, in seconds since the Unix epoch.
    pub close_time: u64,
    /// SHA-256 of the network passphrase.
    pub network_id: [u8; 32],
    /// Hash of the ledger's transaction set; the base of the Soroban PRNG seed.
    pub tx_set_hash: [u8; 32],
}

/// Read the header of the transaction's own ledger: its close time is the
/// timestamp the host sees, and it carries the protocol version and base
/// reserve, unless the ledger upgraded them (see the module docs).
pub async fn read_ledger(
    rpc: &RpcClient,
    sequence: u32,
    passphrase: &str,
) -> Result<LedgerContext> {
    let header = read_header(rpc, sequence).await?;
    let mut protocol = header.ledger_version;
    let mut base_reserve = header.base_reserve;

    let upgraded = Upgraded::by(&header)?;
    if upgraded.protocol || upgraded.base_reserve {
        let previous = match sequence.checked_sub(1) {
            Some(previous) => read_header(rpc, previous).await?,
            None => return Err(RewindError::LedgerUnavailable { sequence }),
        };
        if upgraded.protocol {
            protocol = previous.ledger_version;
        }
        if upgraded.base_reserve {
            base_reserve = previous.base_reserve;
        }
    }

    Ok(LedgerContext {
        sequence,
        protocol,
        base_reserve,
        // The value the host's timestamp is made of, rather than the RPC's
        // rendering of it.
        close_time: header.scp_value.close_time.0,
        network_id: Sha256::digest(passphrase.as_bytes()).into(),
        tx_set_hash: header.scp_value.tx_set_hash.0,
    })
}

async fn read_header(rpc: &RpcClient, sequence: u32) -> Result<LedgerHeader> {
    let ledger = rpc
        .get_ledger(sequence)
        .await?
        .ok_or(RewindError::LedgerUnavailable { sequence })?;
    // The RPC hands back a `LedgerHeaderHistoryEntry` (hash + header), not a
    // bare `LedgerHeader`.
    let history: LedgerHeaderHistoryEntry = codec::decode(&ledger.header_xdr, "headerXdr")?;
    Ok(history.header)
}

/// What a ledger's own upgrades changed, of the values its transactions did
/// not yet run under.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Upgraded {
    protocol: bool,
    base_reserve: bool,
}

impl Upgraded {
    fn by(header: &LedgerHeader) -> Result<Self> {
        let mut upgraded = Upgraded::default();
        for upgrade in header.scp_value.upgrades.iter() {
            let upgrade = LedgerUpgrade::from_xdr(upgrade.0.as_slice(), Limits::none()).map_err(
                |source| RewindError::XdrDecode {
                    what: "a ledger upgrade in headerXdr",
                    source,
                },
            )?;
            match upgrade {
                LedgerUpgrade::Version(_) => upgraded.protocol = true,
                LedgerUpgrade::BaseReserve(_) => upgraded.base_reserve = true,
                _ => {}
            }
        }
        Ok(upgraded)
    }
}

/// Network parameters the host needs that do not live in the transaction.
/// Every value comes from a `ConfigSetting` ledger entry; there are no
/// built-in defaults, because a replay priced with made-up parameters is not
/// comparable with the network's.
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    /// What each host operation costs in instructions.
    pub cpu_cost_params: ContractCostParams,
    /// What each host operation costs in memory.
    pub mem_cost_params: ContractCostParams,
    /// The memory budget of a transaction, in bytes.
    pub tx_memory_limit: u32,
    /// The most contract event bytes one transaction may emit.
    pub tx_max_contract_events_size_bytes: u32,
    /// The longest TTL an entry can be extended to, in ledgers.
    pub max_entry_ttl: u32,
    /// The minimum TTL of a new temporary entry, in ledgers.
    pub min_temporary_ttl: u32,
    /// The minimum TTL of a new persistent entry, in ledgers; also what a
    /// restoration grants.
    pub min_persistent_ttl: u32,
    /// The largest contract code and contract data entries the network takes.
    pub max_contract_size_bytes: u32,
    /// The largest contract data entry, in bytes.
    pub max_contract_data_entry_size_bytes: u32,
    /// The fee settings, for recomputing the transaction's resource fee.
    pub fees: FeeSettings,
    /// Settings modified at or after the transaction's ledger: the network was
    /// reconfigured since, so the replay may be priced differently.
    pub drifted: Vec<&'static str>,
    /// Settings the RPC returned without a `lastModifiedLedgerSeq`: whether
    /// they changed since the transaction cannot be told.
    pub drift_unknown: Vec<&'static str>,
    /// The state-size window, which only prices rent, has moved since the
    /// transaction. It is resampled every few minutes and its average hardly
    /// moves, so this only matters when rent decides the outcome.
    pub state_size_window_drifted: bool,
}

/// The settings the replay depends on, in the order they are requested.
const CONFIG_SETTINGS: [ConfigSettingId; 12] = [
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
];

fn build_config(entries: &[Option<RawLedgerEntry>], tx_ledger: u32) -> Result<NetworkConfig> {
    let mut cpu_cost_params = None;
    let mut mem_cost_params = None;
    let mut compute = None;
    let mut events = None;
    let mut archival = None;
    let mut ledger_cost = None;
    let mut ledger_cost_ext = None;
    let mut historical = None;
    let mut bandwidth = None;
    let mut max_contract_size = None;
    let mut max_data_entry_size = None;
    let mut state_size_window = None;
    let mut drifted = Vec::new();
    let mut drift_unknown = Vec::new();
    let mut state_size_window_drifted = false;

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
            ConfigSettingEntry::ContractLedgerCostV0(c) => ledger_cost = Some(c),
            ConfigSettingEntry::ContractLedgerCostExtV0(c) => ledger_cost_ext = Some(c),
            ConfigSettingEntry::ContractHistoricalDataV0(h) => historical = Some(h),
            ConfigSettingEntry::ContractBandwidthV0(b) => bandwidth = Some(b),
            ConfigSettingEntry::ContractMaxSizeBytes(n) => max_contract_size = Some(n),
            ConfigSettingEntry::ContractDataEntrySizeBytes(n) => max_data_entry_size = Some(n),
            ConfigSettingEntry::LiveSorobanStateSizeWindow(w) => {
                if w.is_empty() {
                    return Err(malformed("the window holds no samples".to_string()));
                }
                state_size_window = Some(w);
            }
            other => return Err(malformed(format!("unexpected setting `{}`", other.name()))),
        }
        let is_window = *id == ConfigSettingId::LiveSorobanStateSizeWindow;
        match raw.last_modified_ledger_seq {
            Some(modified) if modified >= tx_ledger && is_window => {
                state_size_window_drifted = true;
            }
            Some(modified) if modified >= tx_ledger => drifted.push(setting),
            Some(_) => {}
            None if is_window => state_size_window_drifted = true,
            None => drift_unknown.push(setting),
        }
    }

    let missing = |id: ConfigSettingId| RewindError::ConfigSettingMissing { setting: id.name() };
    let compute = compute.ok_or_else(|| missing(ConfigSettingId::ContractComputeV0))?;
    let events = events.ok_or_else(|| missing(ConfigSettingId::ContractEventsV0))?;
    let archival = archival.ok_or_else(|| missing(ConfigSettingId::StateArchival))?;
    let ledger_cost = ledger_cost.ok_or_else(|| missing(ConfigSettingId::ContractLedgerCostV0))?;
    let ledger_cost_ext =
        ledger_cost_ext.ok_or_else(|| missing(ConfigSettingId::ContractLedgerCostExtV0))?;
    let historical =
        historical.ok_or_else(|| missing(ConfigSettingId::ContractHistoricalDataV0))?;
    let bandwidth = bandwidth.ok_or_else(|| missing(ConfigSettingId::ContractBandwidthV0))?;
    let window =
        state_size_window.ok_or_else(|| missing(ConfigSettingId::LiveSorobanStateSizeWindow))?;
    // stellar-core's `mAverageSorobanStateSize`: the integer mean of the window.
    let average_state_size =
        window.iter().map(|size| u128::from(*size)).sum::<u128>() / (window.len().max(1) as u128);

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
        max_contract_size_bytes: max_contract_size
            .ok_or_else(|| missing(ConfigSettingId::ContractMaxSizeBytes))?,
        max_contract_data_entry_size_bytes: max_data_entry_size
            .ok_or_else(|| missing(ConfigSettingId::ContractDataEntrySizeBytes))?,
        fees: FeeSettings {
            fee_per_instruction_increment: compute.fee_rate_per_instructions_increment,
            fee_per_disk_read_entry: ledger_cost.fee_disk_read_ledger_entry,
            fee_per_write_entry: ledger_cost.fee_write_ledger_entry,
            fee_per_disk_read_1kb: ledger_cost.fee_disk_read1_kb,
            fee_per_write_1kb: ledger_cost_ext.fee_write1_kb,
            fee_per_historical_1kb: historical.fee_historical1_kb,
            fee_per_contract_event_1kb: events.fee_contract_events1_kb,
            fee_per_transaction_size_1kb: bandwidth.fee_tx_size1_kb,
            state_target_size_bytes: ledger_cost.soroban_state_target_size_bytes,
            rent_fee_1kb_state_size_low: ledger_cost.rent_fee1_kb_soroban_state_size_low,
            rent_fee_1kb_state_size_high: ledger_cost.rent_fee1_kb_soroban_state_size_high,
            state_size_rent_fee_growth_factor: ledger_cost.soroban_state_rent_fee_growth_factor,
            average_state_size_bytes: i64::try_from(average_state_size).unwrap_or(i64::MAX),
            persistent_rent_rate_denominator: archival.persistent_rent_rate_denominator,
            temporary_rent_rate_denominator: archival.temp_rent_rate_denominator,
        },
        drifted,
        drift_unknown,
        state_size_window_drifted,
    })
}

/// Whether a footprint entry existed, as far as the host is concerned, at the
/// transaction's ledger. See the module docs for the rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// A classic entry; it has no TTL.
    Untracked,
    /// Live when the transaction ran.
    Live {
        /// The TTL handed to the host.
        live_until: u32,
    },
    /// Archived when the transaction ran and listed for automatic restoration.
    Restored {
        /// The TTL core assigns on restoration: `ledger + minPersistentTTL - 1`.
        live_until: u32,
    },
    /// Persistent, past its TTL, and not listed for restoration.
    Archived {
        /// The TTL it had then is lost (`liveUntilLedgerSeq: 0`); that it was
        /// archived is taken from the network's `ENTRY_ARCHIVED` result.
        inferred: bool,
    },
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
    /// Archived since (and possibly restored again), but demonstrably live
    /// when the transaction ran.
    ArchivedSince,
    /// Listed for restoration by the transaction, and restored by something
    /// else since.
    RestoredSince,
}

/// The facts liveness is decided against.
#[derive(Debug, Clone, Copy)]
pub struct LivenessRule {
    /// The ledger the transaction ran in.
    pub tx_ledger: u32,
    /// The network's `maxEntryTTL`, which bounds how far a TTL set by then
    /// could reach.
    pub max_entry_ttl: u32,
    /// The network's `minPersistentTTL`, which sets the TTL of a restoration.
    pub min_persistent_ttl: u32,
}

/// What is known about one durable entry's TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtlFacts {
    /// Temporary or persistent; contract code counts as persistent.
    pub durability: ContractDataDurability,
    /// Its `liveUntilLedgerSeq`; `0` when it is archived now.
    pub live_until: u32,
    /// The transaction listed the entry for automatic restoration.
    pub restorable: bool,
    /// The network may have refused this very entry with `ENTRY_ARCHIVED`.
    pub refused: bool,
    /// `live_until` is the TTL the entry had when the transaction ran, taken
    /// from ledger metadata, rather than the one it has now.
    pub exact: bool,
}

impl LivenessRule {
    /// The TTL core assigns to an entry it restores in the transaction's
    /// ledger.
    fn restored_until(&self) -> u32 {
        self.tx_ledger
            .saturating_add(self.min_persistent_ttl)
            .saturating_sub(1)
    }

    /// Classify one durable entry.
    pub fn classify(&self, facts: TtlFacts) -> (Liveness, Option<TtlDrift>) {
        use ContractDataDurability::{Persistent, Temporary};
        let TtlFacts {
            durability,
            live_until,
            restorable,
            refused,
            exact,
        } = facts;
        let restored = Liveness::Restored {
            live_until: self.restored_until(),
        };

        if live_until >= self.tx_ledger {
            if exact {
                return (Liveness::Live { live_until }, None);
            }
            // The transaction failed and restored nothing, yet the entry it
            // listed is live with a TTL only a later restoration (or an
            // extension on top of one) explains.
            if restorable && durability == Persistent && live_until >= self.restored_until() {
                return (restored, Some(TtlDrift::RestoredSince));
            }
            let furthest_possible = self
                .tx_ledger
                .saturating_add(self.max_entry_ttl)
                .saturating_sub(1);
            let drift = (live_until > furthest_possible).then_some(TtlDrift::ExtendedSince);
            return (Liveness::Live { live_until }, drift);
        }
        match durability {
            Temporary => (Liveness::Expired, None),
            Persistent if restorable => (restored, None),
            Persistent if exact => (Liveness::Archived { inferred: false }, None),
            // `0` means "archived now"; the TTL it had back then is lost. Core
            // refuses archived entries, so unless it refused this one, it was
            // live.
            Persistent if live_until == 0 && !refused => (
                Liveness::Live {
                    live_until: self.tx_ledger,
                },
                Some(TtlDrift::ArchivedSince),
            ),
            Persistent => (
                Liveness::Archived {
                    inferred: live_until == 0,
                },
                None,
            ),
        }
    }
}

/// Which entries the network refused with `ENTRY_ARCHIVED`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Refused {
    /// The network ran the host: it refused nothing.
    Nothing,
    /// `ENTRY_ARCHIVED` without diagnostics: any archived entry may be it.
    Unnamed,
    /// The entry core's diagnostic names.
    Named(LedgerKey),
}

impl Refused {
    fn by_the_network(tx: &DecodedTx) -> Self {
        if tx.onchain_result != InvokeHostFunctionResult::EntryArchived {
            return Refused::Nothing;
        }
        tx.onchain_diagnostic_events
            .iter()
            .find_map(archived_key)
            .map_or(Refused::Unnamed, Refused::Named)
    }

    fn covers(&self, key: &LedgerKey) -> bool {
        match self {
            Refused::Nothing => false,
            Refused::Unnamed => true,
            Refused::Named(named) => named == key,
        }
    }
}

/// The key stellar-core's "trying to access an archived ..." diagnostic names
/// (`InvokeHostFunctionOpFrame::handleArchivedEntry`, checked 2026-11-11).
fn archived_key(event: &DiagnosticEvent) -> Option<LedgerKey> {
    let (topics, data) = display::event_parts(&event.event);
    let [ScVal::Symbol(kind), ScVal::Error(_), ..] = topics else {
        return None;
    };
    if kind.0.as_slice() != b"error" {
        return None;
    }
    let ScVal::Vec(Some(items)) = data else {
        return None;
    };
    let (ScVal::String(message), args) = items.split_first()? else {
        return None;
    };
    match (message.0.as_slice(), args) {
        (b"trying to access an archived contract code entry", [ScVal::Bytes(hash)]) => {
            Some(LedgerKey::ContractCode(LedgerKeyContractCode {
                hash: stellar_xdr::Hash(hash.0.as_slice().try_into().ok()?),
            }))
        }
        (b"trying to access an archived contract data entry", [ScVal::Address(contract), key]) => {
            Some(LedgerKey::ContractData(LedgerKeyContractData {
                contract: contract.clone(),
                key: key.clone(),
                // Only persistent entries are ever archived.
                durability: ContractDataDurability::Persistent,
            }))
        }
        _ => None,
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
    /// The footprint key.
    pub key: LedgerKey,
    /// `key`, XDR-encoded.
    pub key_xdr: Vec<u8>,
    /// The entry under `key`; `None` when the RPC has none, or it did not
    /// exist when the transaction ran.
    pub entry: Option<LedgerEntry>,
    /// `entry`, XDR-encoded once: the bytes the host is handed, and the size
    /// stellar-core's limits are checked against.
    pub entry_xdr: Option<Vec<u8>>,
    /// The entry's `lastModifiedLedgerSeq`; `None` when there is no entry or
    /// the RPC did not say.
    pub last_modified: Option<u32>,
    /// Whether the entry existed for the host, and with which TTL.
    pub liveness: Liveness,
    /// Why its TTL is known to have changed since, when it is.
    pub ttl_drift: Option<TtlDrift>,
    /// The entry (or its absence) is the one the transaction saw, rebuilt
    /// from ledger metadata, rather than the one on the ledger now.
    pub rebuilt: bool,
    /// Ledger metadata shows the entry on the ledger now is the one the
    /// transaction saw, although it was written in the transaction's ledger.
    pub confirmed: bool,
    /// The RPC sent a classic entry without its `extXdr`: it is replayed with
    /// an empty extension, which may make it smaller than it was.
    pub ext_missing: bool,
}

/// What decides one slot, besides the entry itself.
#[derive(Debug, Clone, Copy)]
struct SlotContext<'a> {
    rule: &'a LivenessRule,
    restorable: bool,
    refused: bool,
}

impl FootprintSlot {
    fn new(
        key: LedgerKey,
        key_xdr: Vec<u8>,
        raw: Option<&RawLedgerEntry>,
        past: &Past,
        context: SlotContext<'_>,
    ) -> Result<Self> {
        let durability = durability(&key);
        let mut slot = Self {
            key,
            key_xdr,
            entry: None,
            entry_xdr: None,
            last_modified: None,
            liveness: Liveness::Absent,
            ttl_drift: None,
            rebuilt: false,
            confirmed: false,
            ext_missing: false,
        };

        let mut live_until = raw.and_then(|raw| raw.live_until_ledger_seq);
        if past.restored_since && past.live_until.is_none() {
            // Restored since, so archived by then, and the TTL it had when
            // the transaction ran is lost: it is judged as an entry archived
            // now, not by the TTL the restoration gave it.
            live_until = Some(0);
        }
        if let Some(raw) = raw {
            slot.entry = Some(codec::assemble_entry(raw)?);
            slot.last_modified = raw.last_modified_ledger_seq;
            slot.ext_missing =
                durability.is_none() && raw.ext_xdr.as_deref().is_none_or(str::is_empty);
        }
        match &past.value {
            None => {}
            Some(PastValue::Unchanged) => slot.confirmed = true,
            Some(PastValue::Absent) => {
                slot.entry = None;
                slot.rebuilt = true;
                slot.ext_missing = false;
            }
            // A durable entry that has been deleted since is only worth
            // bringing back when its TTL came back with it.
            Some(PastValue::Entry(_))
                if durability.is_some() && past.live_until.or(live_until).is_none() => {}
            Some(PastValue::Entry(entry)) => {
                slot.last_modified = Some(entry.last_modified_ledger_seq);
                slot.entry = Some((**entry).clone());
                slot.rebuilt = true;
                slot.ext_missing = false;
            }
        }
        let exact = past.live_until.is_some();
        if exact {
            live_until = past.live_until;
        }

        let Some(entry) = &slot.entry else {
            return Ok(slot);
        };
        slot.entry_xdr = Some(codec::encode(entry, "a footprint entry")?);
        (slot.liveness, slot.ttl_drift) = match durability {
            None => (Liveness::Untracked, None),
            Some(durability) => {
                let live_until = live_until.ok_or_else(|| RewindError::MissingTtl {
                    entry: display::ledger_key(&slot.key),
                })?;
                context.rule.classify(TtlFacts {
                    durability,
                    live_until,
                    restorable: context.restorable,
                    refused: context.refused,
                    exact,
                })
            }
        };
        Ok(slot)
    }

    /// The entry was written at or after the transaction's own ledger, so the
    /// value we fetched may not be the value the host saw.
    pub fn value_drifted(&self, tx_ledger: u32) -> bool {
        !self.rebuilt && !self.confirmed && self.last_modified.is_some_and(|m| m >= tx_ledger)
    }

    /// The RPC did not say when the entry was last written, so whether it
    /// changed since the transaction cannot be told.
    pub fn drift_unknown(&self) -> bool {
        self.entry.is_some() && self.last_modified.is_none()
    }

    /// The host is handed no entry for this slot.
    pub fn missing(&self) -> bool {
        matches!(self.liveness, Liveness::Absent | Liveness::Expired)
    }

    /// Archived when the transaction ran and not listed for restoration:
    /// stellar-core fails the operation with `ENTRY_ARCHIVED`.
    pub fn archived(&self) -> bool {
        matches!(self.liveness, Liveness::Archived { .. })
    }

    /// Archived when the transaction ran, and restored by it before the host
    /// sees it.
    pub fn restored(&self) -> bool {
        matches!(self.liveness, Liveness::Restored { .. })
    }

    /// The slot as the host should see it; `None` where it sees no entry.
    fn encoded(&self) -> Option<Entry<'_>> {
        let live_until = match self.liveness {
            Liveness::Absent | Liveness::Expired | Liveness::Archived { .. } => return None,
            Liveness::Untracked => None,
            Liveness::Live { live_until } | Liveness::Restored { live_until } => Some(live_until),
        };
        Some(Entry {
            xdr: self.entry_xdr.as_deref()?,
            live_until,
        })
    }
}

/// The ledger the transaction saw, as far as it can be rebuilt: everything
/// the host is handed besides the transaction itself.
#[derive(Debug)]
pub struct Reconstructed {
    /// The ledger the host runs in.
    pub ledger_info: LedgerInfo,
    /// The network's settings, from its `ConfigSetting` entries.
    pub config: NetworkConfig,
    /// In footprint order: read-only keys first, then read-write.
    pub slots: Vec<FootprintSlot>,
    /// Number of read-only keys at the front of `slots`.
    pub read_only: usize,
    /// Why drifted entries could not be rebuilt from ledger metadata, when
    /// some could not.
    pub rebuild_note: Option<String>,
}

impl Reconstructed {
    /// Encode the slots exactly as `invoke_host_function` wants them: one item
    /// per footprint key, `None` where the host should see no entry.
    pub fn encoded_entries(&self) -> Vec<Option<Entry<'_>>> {
        self.slots.iter().map(FootprintSlot::encoded).collect()
    }

    /// Look an entry up among the footprint slots by its encoded key.
    pub fn slot(&self, key_xdr: &[u8]) -> Option<&FootprintSlot> {
        self.slots.iter().find(|slot| slot.key_xdr == key_xdr)
    }

    /// Indices into the read-write footprint of the entries core restores
    /// before it invokes the host: the listed ones it finds archived.
    pub fn restored_rw_indices(&self) -> Vec<u32> {
        self.slots
            .iter()
            .skip(self.read_only)
            .zip(0u32..)
            .filter(|(slot, _)| slot.restored())
            .map(|(_, index)| index)
            .collect()
    }
}

/// What `getLedgerEntries` says about the footprint and the network's
/// settings now. Fetching it needs nothing but the transaction, so it runs
/// alongside [`read_ledger`].
#[derive(Debug)]
pub struct Fetched {
    footprint: Vec<LedgerKey>,
    key_xdr: Vec<Vec<u8>>,
    entries: Vec<Option<RawLedgerEntry>>,
    config_entries: Vec<Option<RawLedgerEntry>>,
}

/// Fetch every footprint entry and the network's settings in one
/// `getLedgerEntries` request.
pub async fn fetch(rpc: &RpcClient, tx: &DecodedTx) -> Result<Fetched> {
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
    Ok(Fetched {
        footprint,
        key_xdr,
        entries,
        config_entries,
    })
}

/// Turn what was fetched into the ledger the transaction saw.
pub async fn reconstruct(
    rpc: &RpcClient,
    tx: &DecodedTx,
    ledger: &LedgerContext,
    fetched: Fetched,
) -> Result<Reconstructed> {
    let Fetched {
        footprint,
        key_xdr,
        entries,
        config_entries,
    } = fetched;
    let config = build_config(&config_entries, ledger.sequence)?;

    let Rebuild { past, note } = history::rebuild(
        rpc,
        &history::Target {
            ledger: ledger.sequence,
            application_order: tx.application_order,
        },
        &key_xdr,
        &entries,
    )
    .await;

    let rule = LivenessRule {
        tx_ledger: ledger.sequence,
        max_entry_ttl: config.max_entry_ttl,
        min_persistent_ttl: config.min_persistent_ttl,
    };
    let refused = Refused::by_the_network(tx);
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
        .zip(entries.iter().zip(&past))
        .enumerate()
        .map(|(i, ((key, key_xdr), (raw, past)))| {
            let context = SlotContext {
                rule: &rule,
                restorable: restorable.contains(&i),
                refused: refused.covers(&key),
            };
            FootprintSlot::new(key, key_xdr, raw.as_ref(), past, context)
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
        read_only,
        rebuild_note: note,
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
        }
    }

    /// Classify from the entry's current TTL, the network having run the host.
    fn classify(
        durability: ContractDataDurability,
        live_until: u32,
        restorable: bool,
    ) -> (Liveness, Option<TtlDrift>) {
        rule().classify(TtlFacts {
            durability,
            live_until,
            restorable,
            refused: false,
            exact: false,
        })
    }

    const ARCHIVED: Liveness = Liveness::Archived { inferred: false };

    #[test]
    fn live_entries_keep_their_ttl() {
        for durability in [Persistent, Temporary] {
            // Live through the transaction's own ledger, inclusive.
            assert_eq!(
                classify(durability, TX_LEDGER, false),
                (
                    Liveness::Live {
                        live_until: TX_LEDGER
                    },
                    None
                )
            );
            assert_eq!(
                classify(durability, TX_LEDGER + 499, false),
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
            classify(Persistent, TX_LEDGER + 500, false),
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
            classify(Temporary, TX_LEDGER - 1, false),
            (Liveness::Expired, None)
        );
        assert_eq!(classify(Temporary, 0, false), (Liveness::Expired, None));
        // Being listed for restoration does not bring a temporary entry back.
        assert_eq!(
            classify(Temporary, TX_LEDGER - 1, true),
            (Liveness::Expired, None)
        );
    }

    #[test]
    fn archived_persistent_entries_need_restoration() {
        assert_eq!(classify(Persistent, TX_LEDGER - 1, false), (ARCHIVED, None));
        let restored = Liveness::Restored {
            live_until: TX_LEDGER + 99,
        };
        assert_eq!(classify(Persistent, TX_LEDGER - 1, true), (restored, None));
        assert_eq!(classify(Persistent, 0, true), (restored, None));
    }

    #[test]
    fn a_listed_entry_that_is_live_now_is_judged_by_its_ttl() {
        // A TTL below what a restoration in the transaction's ledger would
        // set was set before it: the entry was simply live, and core does not
        // restore a live entry.
        assert_eq!(
            classify(Persistent, TX_LEDGER + 98, true),
            (
                Liveness::Live {
                    live_until: TX_LEDGER + 98
                },
                None
            )
        );
        // From there on, a later restoration explains it: the transaction
        // failed, so it was not this one.
        for live_until in [TX_LEDGER + 99, TX_LEDGER + 400, TX_LEDGER + 9_000] {
            assert_eq!(
                classify(Persistent, live_until, true),
                (
                    Liveness::Restored {
                        live_until: TX_LEDGER + 99
                    },
                    Some(TtlDrift::RestoredSince)
                )
            );
        }
    }

    #[test]
    fn archived_now_is_judged_by_what_the_network_recorded() {
        // The network ran the host, so the entry was live back then.
        assert_eq!(
            classify(Persistent, 0, false),
            (
                Liveness::Live {
                    live_until: TX_LEDGER
                },
                Some(TtlDrift::ArchivedSince)
            )
        );
        // The network refused it with ENTRY_ARCHIVED: it was archived then
        // too, which is inferred rather than read from a TTL.
        let refused = |live_until| {
            rule()
                .classify(TtlFacts {
                    durability: Persistent,
                    live_until,
                    restorable: false,
                    refused: true,
                    exact: false,
                })
                .0
        };
        assert_eq!(refused(0), Liveness::Archived { inferred: true });
        // A TTL that ran out before the transaction proves it.
        assert_eq!(refused(TX_LEDGER - 1), ARCHIVED);
    }

    #[test]
    fn a_ttl_rebuilt_from_metadata_is_taken_at_its_word() {
        let exact = |live_until, restorable| {
            rule().classify(TtlFacts {
                durability: Persistent,
                live_until,
                restorable,
                refused: false,
                exact: true,
            })
        };
        // No heuristics: beyond the maximum, or listed and live, is just live.
        for live_until in [TX_LEDGER, TX_LEDGER + 9_000] {
            assert_eq!(
                exact(live_until, true),
                (Liveness::Live { live_until }, None)
            );
        }
        assert_eq!(exact(0, false), (ARCHIVED, None));
        assert_eq!(
            exact(TX_LEDGER - 1, true),
            (
                Liveness::Restored {
                    live_until: TX_LEDGER + 99
                },
                None
            )
        );
    }

    #[test]
    fn the_refused_entry_is_the_one_core_names() {
        use crate::test_support::{decoded_tx, diag, string, sym};
        use stellar_xdr::{ScError, ScErrorCode};

        let named = data_entry([2; 32], Persistent, 10);
        let other = data_entry([3; 32], Persistent, 10);
        let (named_key, other_key) = (
            crate::test_support::key_of(&named),
            crate::test_support::key_of(&other),
        );
        let LedgerKey::ContractData(data) = &named_key else {
            panic!("a contract data key");
        };
        let event = diag(
            None,
            vec![
                sym("error"),
                ScVal::Error(ScError::Value(ScErrorCode::InvalidInput)),
            ],
            ScVal::Vec(Some(
                vec![
                    string("trying to access an archived contract data entry"),
                    ScVal::Address(data.contract.clone()),
                    data.key.clone(),
                ]
                .try_into()
                .unwrap(),
            )),
            false,
        );

        let mut tx = decoded_tx();
        assert_eq!(Refused::by_the_network(&tx), Refused::Nothing);
        tx.onchain_result = InvokeHostFunctionResult::EntryArchived;
        let unnamed = Refused::by_the_network(&tx);
        assert!(unnamed.covers(&named_key) && unnamed.covers(&other_key));

        tx.onchain_diagnostic_events = vec![event];
        let refused = Refused::by_the_network(&tx);
        assert!(refused.covers(&named_key));
        assert!(!refused.covers(&other_key));

        // Contract code is named by its hash.
        let code = code_entry(b"wasm", 10);
        let code_key = crate::test_support::key_of(&code);
        let LedgerKey::ContractCode(code) = &code_key else {
            panic!("a contract code key");
        };
        tx.onchain_diagnostic_events = vec![diag(
            None,
            vec![
                sym("error"),
                ScVal::Error(ScError::Value(ScErrorCode::InvalidInput)),
            ],
            ScVal::Vec(Some(
                vec![
                    string("trying to access an archived contract code entry"),
                    ScVal::Bytes(code.hash.0.to_vec().try_into().unwrap()),
                ]
                .try_into()
                .unwrap(),
            )),
            false,
        )];
        assert!(Refused::by_the_network(&tx).covers(&code_key));
    }

    fn slot_of(
        entry: &LedgerEntry,
        live_until: Option<u32>,
        restorable: bool,
    ) -> Result<FootprintSlot> {
        let raw = raw_entry(entry, live_until);
        let key = crate::test_support::key_of(entry);
        let context = SlotContext {
            rule: &rule(),
            restorable,
            refused: false,
        };
        FootprintSlot::new(key, vec![1], Some(&raw), &Past::default(), context)
    }

    #[test]
    fn slots_follow_the_liveness_table() {
        // Classic entry: handed over without TTL metadata.
        let classic = slot_of(&account_entry([1; 32], 10), None, false).unwrap();
        assert_eq!(classic.liveness, Liveness::Untracked);
        assert_eq!(classic.encoded().unwrap().live_until, None);

        // Live durable entry: entry plus TTL.
        let live = slot_of(
            &data_entry([2; 32], Persistent, 10),
            Some(TX_LEDGER + 5),
            false,
        )
        .unwrap();
        assert_eq!(live.encoded().unwrap().live_until, Some(TX_LEDGER + 5));
        assert!(!live.missing() && !live.archived() && !live.restored());

        // Expired temporary entry: nothing.
        let expired = slot_of(
            &data_entry([2; 32], Temporary, 10),
            Some(TX_LEDGER - 1),
            false,
        )
        .unwrap();
        assert!(expired.missing());
        assert_eq!(expired.encoded(), None);

        // Archived code entry: flagged, and never handed to the host.
        let archived = slot_of(&code_entry(b"wasm", 10), Some(TX_LEDGER - 1), false).unwrap();
        assert!(archived.archived());
        assert_eq!(archived.encoded(), None);

        // Restorable archived entry: handed over with the restored TTL.
        let restored = slot_of(&code_entry(b"wasm", 10), Some(TX_LEDGER - 1), true).unwrap();
        assert!(restored.restored());
        assert_eq!(restored.encoded().unwrap().live_until, Some(TX_LEDGER + 99));

        // Absent entry.
        let key = crate::test_support::key_of(&account_entry([1; 32], 10));
        let context = SlotContext {
            rule: &rule(),
            restorable: false,
            refused: false,
        };
        let absent = FootprintSlot::new(key, vec![1], None, &Past::default(), context).unwrap();
        assert_eq!(absent.liveness, Liveness::Absent);
        assert!(absent.missing());
    }

    #[test]
    fn only_entries_found_archived_are_restored() {
        let slot = |entry: &LedgerEntry, live_until, restorable| {
            slot_of(entry, Some(live_until), restorable).unwrap()
        };
        let state = Reconstructed {
            ledger_info: LedgerInfo::default(),
            config: build_config(&config_entries(1), TX_LEDGER).unwrap(),
            slots: vec![
                slot(&code_entry(b"wasm", 10), TX_LEDGER + 5, false),
                // Listed, but live with a TTL set before the transaction.
                slot(&data_entry([2; 32], Persistent, 10), TX_LEDGER + 5, true),
                slot(&data_entry([3; 32], Persistent, 10), TX_LEDGER - 1, true),
            ],
            read_only: 1,
            rebuild_note: None,
        };
        // Core tells the host about the second read-write entry only.
        assert_eq!(state.restored_rw_indices(), [1]);
    }

    #[test]
    fn a_durable_entry_without_ttl_is_an_error_that_names_it() {
        let entry = data_entry([2; 32], Persistent, 10);
        let err = slot_of(&entry, None, false).unwrap_err();
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
            slot_of(&account_entry([1; 32], last_modified), None, false)
                .unwrap()
                .value_drifted(TX_LEDGER)
        };
        assert!(!drifted(TX_LEDGER - 1));
        assert!(drifted(TX_LEDGER));
        assert!(drifted(TX_LEDGER + 1));
    }

    #[test]
    fn what_the_rpc_leaves_out_is_reported_not_assumed() {
        let entry = account_entry([1; 32], 10);
        let key = crate::test_support::key_of(&entry);
        let context = SlotContext {
            rule: &rule(),
            restorable: false,
            refused: false,
        };
        let mut raw = raw_entry(&entry, None);
        raw.last_modified_ledger_seq = None;
        raw.ext_xdr = None;
        let slot = FootprintSlot::new(key, vec![1], Some(&raw), &Past::default(), context).unwrap();
        // Neither "drifted" nor "not drifted": unknown.
        assert!(!slot.value_drifted(TX_LEDGER));
        assert!(slot.drift_unknown());
        assert!(slot.ext_missing);

        // A Soroban entry has no extension to lose.
        let data = slot_of(&data_entry([2; 32], Persistent, 10), Some(TX_LEDGER), false).unwrap();
        assert!(!data.ext_missing && !data.drift_unknown());
    }

    #[test]
    fn a_rebuilt_entry_replaces_the_current_one() {
        let now = data_entry([2; 32], Persistent, TX_LEDGER + 7);
        let then = {
            let mut entry = now.clone();
            entry.last_modified_ledger_seq = TX_LEDGER - 3;
            entry
        };
        let key = crate::test_support::key_of(&now);
        let context = SlotContext {
            rule: &rule(),
            restorable: false,
            refused: false,
        };
        let raw = raw_entry(&now, Some(TX_LEDGER + 9_000));

        let past = Past {
            value: Some(PastValue::Entry(Box::new(then.clone()))),
            live_until: Some(TX_LEDGER + 20),
            restored_since: false,
        };
        let slot = FootprintSlot::new(key.clone(), vec![1], Some(&raw), &past, context).unwrap();
        assert_eq!(slot.entry.as_ref(), Some(&then));
        assert!(slot.rebuilt && !slot.value_drifted(TX_LEDGER));
        // The TTL it had then, and no drift to report.
        assert_eq!(
            (slot.liveness, slot.ttl_drift),
            (
                Liveness::Live {
                    live_until: TX_LEDGER + 20
                },
                None
            )
        );

        // Created after the transaction: it did not exist.
        let past = Past {
            value: Some(PastValue::Absent),
            live_until: None,
            restored_since: false,
        };
        let slot = FootprintSlot::new(key, vec![1], Some(&raw), &past, context).unwrap();
        assert!(slot.missing() && slot.rebuilt && slot.entry.is_none());
    }

    #[test]
    fn an_entry_restored_since_is_judged_as_archived_now() {
        // Restored after the transaction: the archived value, and the TTL the
        // restoration set, which says nothing about the TTL back then.
        let now = data_entry([2; 32], Persistent, TX_LEDGER + 7);
        let then = {
            let mut entry = now.clone();
            entry.last_modified_ledger_seq = TX_LEDGER - 700;
            entry
        };
        let key = crate::test_support::key_of(&now);
        let raw = raw_entry(&now, Some(TX_LEDGER + 9_000));
        let past = Past {
            value: Some(PastValue::Entry(Box::new(then.clone()))),
            live_until: None,
            restored_since: true,
        };
        let slot = |restorable, refused| {
            let context = SlotContext {
                rule: &rule(),
                restorable,
                refused,
            };
            FootprintSlot::new(key.clone(), vec![1], Some(&raw), &past, context).unwrap()
        };

        // The network ran the host, so it was live then, and archived since.
        let live = slot(false, false);
        assert_eq!(live.entry.as_ref(), Some(&then));
        assert!(live.rebuilt);
        assert_eq!(
            (live.liveness, live.ttl_drift),
            (
                Liveness::Live {
                    live_until: TX_LEDGER
                },
                Some(TtlDrift::ArchivedSince)
            )
        );
        // The network refused it: archived then, as the network recorded.
        let refused = slot(false, true);
        assert_eq!(refused.liveness, Liveness::Archived { inferred: true });
        assert!(refused.archived());
        // Listed for restoration: archived then, and restored by core.
        let restored = slot(true, false);
        assert!(restored.restored());
        assert_eq!(restored.encoded().unwrap().live_until, Some(TX_LEDGER + 99));

        // A TTL change recorded before the restoration is the TTL it had.
        let past = Past {
            live_until: Some(TX_LEDGER + 20),
            ..past.clone()
        };
        let context = SlotContext {
            rule: &rule(),
            restorable: false,
            refused: false,
        };
        let slot = FootprintSlot::new(key, vec![1], Some(&raw), &past, context).unwrap();
        assert_eq!(
            (slot.liveness, slot.ttl_drift),
            (
                Liveness::Live {
                    live_until: TX_LEDGER + 20
                },
                None
            )
        );
    }

    fn header(version: u32, base_reserve: u32, upgrades: &[LedgerUpgrade]) -> LedgerHeader {
        use stellar_xdr::WriteXdr;
        let mut header = LedgerHeader {
            ledger_version: version,
            base_reserve,
            ..LedgerHeader::default()
        };
        header.scp_value.upgrades = upgrades
            .iter()
            .map(|upgrade| {
                stellar_xdr::UpgradeType(
                    upgrade.to_xdr(Limits::none()).unwrap().try_into().unwrap(),
                )
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        header.scp_value.close_time = stellar_xdr::TimePoint(1_700_000_000);
        header
    }

    fn ledger_response(sequence: u32, header: &LedgerHeader) -> serde_json::Value {
        use stellar_xdr::WriteXdr;
        let history = LedgerHeaderHistoryEntry {
            header: header.clone(),
            ..LedgerHeaderHistoryEntry::default()
        };
        serde_json::json!({"result": {"ledgers": [{
            "sequence": sequence,
            "headerXdr": codec::b64_encode(&history.to_xdr(Limits::none()).unwrap()),
        }]}})
    }

    #[tokio::test]
    async fn a_ledger_runs_under_the_values_its_upgrades_replace() {
        use crate::test_support::MemoryTransport;

        // Ledger 100 upgraded the protocol and the reserve; its transactions
        // ran under ledger 99's.
        let upgraded = header(
            30,
            7,
            &[LedgerUpgrade::Version(30), LedgerUpgrade::BaseReserve(7)],
        );
        let previous = header(29, 5, &[]);
        let transport = MemoryTransport::new().on_fn("getLedgers", move |params| {
            match params["startLedger"].as_u64() {
                Some(100) => ledger_response(100, &upgraded),
                Some(99) => ledger_response(99, &previous),
                other => panic!("unexpected ledger {other:?}"),
            }
        });
        let calls = transport.calls();
        let ledger = read_ledger(&RpcClient::new(transport), 100, "p")
            .await
            .unwrap();
        assert_eq!((ledger.protocol, ledger.base_reserve), (29, 5));
        // The close time is the header's own.
        assert_eq!(ledger.close_time, 1_700_000_000);
        assert_eq!(calls.lock().unwrap().len(), 2);

        // Other upgrades do not change what the transactions ran under, and
        // cost no second request.
        let plain = header(30, 7, &[LedgerUpgrade::BaseFee(200)]);
        let transport =
            MemoryTransport::new().on_fn("getLedgers", move |_| ledger_response(100, &plain));
        let calls = transport.calls();
        let ledger = read_ledger(&RpcClient::new(transport), 100, "p")
            .await
            .unwrap();
        assert_eq!((ledger.protocol, ledger.base_reserve), (30, 7));
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn builds_config_from_complete_settings() {
        let config = build_config(&config_entries(TX_LEDGER - 1), TX_LEDGER).unwrap();
        assert_eq!(config.tx_memory_limit, 41_943_040);
        assert_eq!(config.tx_max_contract_events_size_bytes, 16_384);
        assert_eq!(config.max_entry_ttl, 3_110_400);
        assert_eq!(config.min_temporary_ttl, 17_280);
        assert_eq!(config.min_persistent_ttl, 2_073_600);
        assert_eq!(config.max_contract_size_bytes, 131_072);
        assert_eq!(config.max_contract_data_entry_size_bytes, 65_536);
        assert_eq!(config.fees.fee_per_write_1kb, 3_500);
        assert_eq!(config.fees.fee_per_transaction_size_1kb, 1_624);
        // The integer mean of the state-size window, as core computes it.
        assert_eq!(config.fees.average_state_size_bytes, 1_000_000_001);
        assert!(config.drifted.is_empty() && config.drift_unknown.is_empty());
        assert!(!config.state_size_window_drifted);
    }

    #[test]
    fn config_modified_since_the_transaction_is_drift() {
        let config = build_config(&config_entries(TX_LEDGER), TX_LEDGER).unwrap();
        // The state-size window moves every few minutes and only prices rent:
        // it is tracked apart from the settings that reprice the whole replay.
        assert_eq!(config.drifted.len(), CONFIG_SETTINGS.len() - 1);
        assert!(config.drifted.contains(&"StateArchival"));
        assert!(!config.drifted.contains(&"LiveSorobanStateSizeWindow"));
        assert!(config.state_size_window_drifted);
    }

    #[test]
    fn config_without_a_modification_ledger_is_unknown_drift() {
        let mut entries = config_entries(1);
        for entry in entries.iter_mut().flatten() {
            entry.last_modified_ledger_seq = None;
        }
        let config = build_config(&entries, TX_LEDGER).unwrap();
        assert!(config.drifted.is_empty());
        assert_eq!(config.drift_unknown.len(), CONFIG_SETTINGS.len() - 1);
        assert!(config.state_size_window_drifted);
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
