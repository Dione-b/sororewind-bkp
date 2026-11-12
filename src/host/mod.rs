//! The Soroban hosts this build embeds, one per protocol it can replay.
//!
//! A host refuses to execute a ledger from any protocol but its own, and
//! testnet upgrades weeks before mainnet, so one host can only ever serve one
//! of them. Like stellar-core across an upgrade, sororewind links the previous
//! and the current host side by side and picks one per transaction from the
//! ledger's protocol.
//!
//! Each host has its own `stellar-xdr`, so its types never leave this module.
//! The host-facing code is written once, in `proto.rs`, and compiled against
//! each host by `p29.rs` and `p30.rs`; everything crossing the boundary is XDR
//! bytes or the plain data below. The rest of the crate stays on the newest
//! `stellar-xdr`: XDR only ever grows, so it decodes anything an older
//! protocol produced, and what a ledger of that protocol holds re-encodes for
//! its host unchanged.
//!
//! Adding a protocol: add a `pNN.rs` mounting `proto.rs`, its renamed
//! dependency in `Cargo.toml` and an entry in [`HOSTS`]; drop the oldest one
//! at the same time. See CONTRIBUTING.md, "Protocol upgrades".

mod p29;
mod p30;

/// The ledger the transaction ran in, as every host's `LedgerInfo` holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerInfo {
    /// The ledger's protocol, which picks the host that replays it.
    pub protocol_version: u32,
    /// The ledger sequence number.
    pub sequence_number: u32,
    /// The ledger's close time, in seconds since the Unix epoch.
    pub timestamp: u64,
    /// SHA-256 of the network passphrase.
    pub network_id: [u8; 32],
    /// The base reserve, in stroops.
    pub base_reserve: u32,
    /// The minimum TTL of a new temporary entry, in ledgers.
    pub min_temp_entry_ttl: u32,
    /// The minimum TTL of a new persistent entry, in ledgers.
    pub min_persistent_entry_ttl: u32,
    /// The longest TTL an entry can be extended to, in ledgers.
    pub max_entry_ttl: u32,
}

/// A footprint entry the host should see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry<'a> {
    /// The `LedgerEntry`, XDR-encoded.
    pub xdr: &'a [u8],
    /// Its TTL, for contract data and code; `None` for classic entries.
    pub live_until: Option<u32>,
}

/// A contract to parse into the module cache before the invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractCode<'a> {
    /// The SHA-256 of the Wasm, which keys the module cache.
    pub hash: [u8; 32],
    /// Borrowed from the footprint entry: a contract can be 128 KB.
    pub wasm: &'a [u8],
    /// `ContractCodeCostInputs`, XDR-encoded, when the entry records them.
    pub cost_inputs: Option<Vec<u8>>,
}

/// Everything one `invoke_host_function` call needs.
#[derive(Debug)]
pub struct Invocation<'a> {
    /// `HostFunction`, `SorobanResources`, `AccountId` and each
    /// `SorobanAuthorizationEntry`, XDR-encoded.
    pub host_function: Vec<u8>,
    /// The transaction's declared `SorobanResources`, footprint included.
    pub resources: Vec<u8>,
    /// The `AccountId` of the operation's source account.
    pub source_account: Vec<u8>,
    /// One `SorobanAuthorizationEntry` per item.
    pub auth: Vec<Vec<u8>>,
    /// Indices into the read-write footprint of the archived entries core
    /// restores before it invokes the host.
    pub restored_rw_indices: &'a [u32],
    /// The ledger the transaction ran in.
    pub ledger: LedgerInfo,
    /// One item per footprint key, in footprint order; `None` where the host
    /// should see no entry.
    pub entries: Vec<Option<Entry<'a>>>,
    /// The base PRNG seed core derives for this transaction.
    pub prng_seed: [u8; 32],
    /// The budget's instruction limit: the transaction's declared instructions.
    pub cpu_limit: u64,
    /// The budget's memory limit, in bytes: the network's `txMemoryLimit`.
    pub mem_limit: u64,
    /// The network's `ContractCostParams`, XDR-encoded.
    pub cpu_cost_params: Vec<u8>,
    /// The memory counterpart of `cpu_cost_params`, XDR-encoded.
    pub mem_cost_params: Vec<u8>,
    /// The contracts in the footprint, to parse into the module cache.
    pub contracts: Vec<ContractCode<'a>>,
}

/// What one footprint entry became.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Change {
    /// Whether the entry is in the read-only footprint.
    pub read_only: bool,
    /// The new `LedgerEntry`, XDR-encoded; `None` when it was removed or is
    /// read-only.
    pub new_value: Option<Vec<u8>>,
    /// What the change costs in rent, when it costs anything.
    pub rent: Option<RentChange>,
}

/// One entry's rent inputs, as the host's `LedgerEntryRentChange` holds them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RentChange {
    /// Whether the entry is persistent rather than temporary.
    pub is_persistent: bool,
    /// Whether the entry is contract code.
    pub is_code_entry: bool,
    /// Its rent size before the invocation; 0 for a new entry.
    pub old_size_bytes: u32,
    /// Its rent size after the invocation.
    pub new_size_bytes: u32,
    /// Its TTL before the invocation; 0 for a new entry.
    pub old_live_until_ledger: u32,
    /// Its TTL after the invocation.
    pub new_live_until_ledger: u32,
}

/// The network's fee settings, as the hosts' `fees` module wants them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeeSettings {
    /// The fee per 10,000 instructions, in stroops.
    pub fee_per_instruction_increment: i64,
    /// The fee per ledger entry read from disk, in stroops.
    pub fee_per_disk_read_entry: i64,
    /// The fee per ledger entry written, in stroops.
    pub fee_per_write_entry: i64,
    /// The fee per 1 KB read from disk, in stroops.
    pub fee_per_disk_read_1kb: i64,
    /// The flat write fee (`feeWrite1KB`).
    pub fee_per_write_1kb: i64,
    /// The fee per 1 KB of history archived for the transaction, in stroops.
    pub fee_per_historical_1kb: i64,
    /// The fee per 1 KB of contract events, in stroops.
    pub fee_per_contract_event_1kb: i64,
    /// The fee per 1 KB of transaction size, in stroops.
    pub fee_per_transaction_size_1kb: i64,
    /// The inputs of the rent fee per 1 KB, which grows with the state size.
    pub state_target_size_bytes: i64,
    /// The rent fee per 1 KB when the state is empty, in stroops.
    pub rent_fee_1kb_state_size_low: i64,
    /// The rent fee per 1 KB when the state reaches the target size, in stroops.
    pub rent_fee_1kb_state_size_high: i64,
    /// How much faster the rent fee grows once the state exceeds the target.
    pub state_size_rent_fee_growth_factor: u32,
    /// Average of the network's `LiveSorobanStateSizeWindow`.
    pub average_state_size_bytes: i64,
    /// The denominator of the rent rate of persistent entries.
    pub persistent_rent_rate_denominator: i64,
    /// The denominator of the rent rate of temporary entries.
    pub temporary_rent_rate_denominator: i64,
}

/// What a transaction declared and what it emitted, for the fee computation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeeRequest {
    /// The declared instruction limit.
    pub instructions: u32,
    /// Entries read from disk: classic ones and those listed for restoration.
    pub disk_read_entries: u32,
    /// Entries in the read-write footprint.
    pub write_entries: u32,
    /// The declared disk read bytes.
    pub disk_read_bytes: u32,
    /// The declared write bytes.
    pub write_bytes: u32,
    /// The size of the emitted contract events, XDR-encoded.
    pub contract_events_size_bytes: u32,
    /// The size of the transaction envelope, XDR-encoded.
    pub transaction_size_bytes: u32,
    /// The rent inputs of every footprint entry whose change costs rent.
    pub rent_changes: Vec<RentChange>,
    /// The ledger the transaction ran in, which rent is counted from.
    pub ledger_sequence: u32,
    /// The network's fee settings at that ledger.
    pub settings: FeeSettings,
}

/// The fees stellar-core derives for one transaction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fees {
    /// Charged whatever happens; depends only on the declared resources.
    pub non_refundable: i64,
    /// The refundable part of the resource fee: the events fee.
    pub refundable: i64,
    /// The rent fee for the entries the transaction created, grew or extended.
    pub rent: i64,
}

/// What the host reported for one invocation.
#[derive(Debug)]
pub struct Output {
    /// The return `ScVal`, XDR-encoded; on failure, the `ScError` behind it
    /// when the host error carries one.
    pub result: std::result::Result<Vec<u8>, Option<Vec<u8>>>,
    /// One item per footprint entry, in footprint order; empty on failure.
    pub changes: Vec<Change>,
    /// Each `ContractEvent`, XDR-encoded.
    pub contract_events: Vec<Vec<u8>>,
    /// Each `DiagnosticEvent`, XDR-encoded.
    pub diagnostics: Vec<Vec<u8>>,
    /// The instructions the budget consumed.
    pub cpu_insns: u64,
    /// The memory the budget consumed, in bytes.
    pub mem_bytes: u64,
}

/// Why a host could not run the invocation at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// A value did not round-trip through the host's XDR.
    Xdr(String),
    /// The budget could not be built from the network's cost parameters, or
    /// could not report what it consumed.
    Budget(String),
    /// The module cache could not be set up.
    ModuleCache(String),
    /// The host failed with an internal error, which stellar-core treats as
    /// a bug rather than as a transaction result.
    Start(String),
    /// The footprint entry at this index could not be sized for rent.
    Rent(usize),
}

/// One embedded host.
pub struct Host {
    /// The protocol it executes, and the only one it accepts.
    pub protocol: u32,
    /// Its `soroban-env-host` version.
    pub version: &'static str,
    invoke: fn(&Invocation<'_>) -> std::result::Result<Output, Failure>,
    fees: fn(&FeeRequest) -> Fees,
}

impl Host {
    /// Run the invocation through this host's `invoke_host_function`.
    pub fn invoke(&self, invocation: &Invocation<'_>) -> std::result::Result<Output, Failure> {
        (self.invoke)(invocation)
    }

    /// The resource and rent fees, computed by this host's own `fees` module.
    pub fn fees(&self, request: &FeeRequest) -> Fees {
        (self.fees)(request)
    }
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Host")
            .field("protocol", &self.protocol)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// Every embedded host, oldest protocol first.
pub static HOSTS: [Host; 2] = [
    Host {
        protocol: p29::PROTOCOL,
        version: p29::VERSION,
        invoke: p29::invoke,
        fees: p29::fees,
    },
    Host {
        protocol: p30::PROTOCOL,
        version: p30::VERSION,
        invoke: p30::invoke,
        fees: p30::fees,
    },
];

/// The host for a ledger of `protocol`, if this build embeds one.
pub fn for_protocol(protocol: u32) -> Option<&'static Host> {
    HOSTS.iter().find(|host| host.protocol == protocol)
}

/// The host for the newest protocol this build replays.
pub fn newest() -> &'static Host {
    &HOSTS[HOSTS.len() - 1]
}

/// The protocols this build replays, oldest first.
pub fn protocols() -> impl Iterator<Item = u32> {
    HOSTS.iter().map(|host| host.protocol)
}

/// "29 (soroban-env-host 29.0.2), 30 (soroban-env-host 30.0.0)".
pub fn describe() -> String {
    HOSTS
        .iter()
        .map(|host| format!("{} (soroban-env-host {})", host.protocol, host.version))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_sorted_and_each_runs_its_own_protocol() {
        let protocols: Vec<u32> = protocols().collect();
        assert_eq!(protocols, [29, 30]);
        for host in &HOSTS {
            // A host's major version is the protocol it executes.
            let major: u32 = host.version.split('.').next().unwrap().parse().unwrap();
            assert_eq!(major, host.protocol, "{host:?}");
            assert_eq!(for_protocol(host.protocol).unwrap().version, host.version);
        }
        assert_eq!(newest().protocol, 30);
        assert!(for_protocol(28).is_none());
        assert!(for_protocol(31).is_none());
    }
}
