//! history: rebuild the entries the transaction saw from ledger metadata.
//!
//! `getLedgerEntries` only serves the *current* value of an entry. A contract
//! that rewrites the same entries every few ledgers has left the state the
//! transaction ran against behind within seconds, and a replay against today's
//! values diverges. The values are not lost, though: `getLedgers` returns each
//! ledger's `LedgerCloseMeta`, and every change it records to an existing
//! entry is preceded by a `state` record holding the entry as it was before
//! the change.
//!
//! # How a past value is rebuilt
//!
//! The value an entry had when the transaction ran is the `state` record of
//! the **first change made to it after the transaction**. Changes are ordered
//! the way stellar-core applies them (checked 2026-11-11 against stellar-core
//! and against recorded `LedgerCloseMeta` V2 with `TransactionMeta` V3 and V4,
//! protocols 29 and 30):
//!
//! 1. `feeProcessing` of every transaction, in apply order: all fees are
//!    charged before anything is applied;
//! 2. per transaction, in apply order: `txChangesBefore` (the sequence-number
//!    bump), the operations' changes, `txChangesAfter`;
//! 3. `postTxApplyFeeProcessing` of every transaction (Soroban refunds);
//! 4. `upgradesProcessing`.
//!
//! The host runs between a transaction's `txChangesBefore` and its
//! operations, so "after the transaction" starts there. The transaction's
//! position is `applicationOrder - 1`, as for the PRNG seed.
//!
//! What the first later change is decides the past value:
//!
//! | first later change      | the entry, when the transaction ran            |
//! |-------------------------|------------------------------------------------|
//! | `state` (then `updated` or `removed`) | the recorded state               |
//! | `created`               | did not exist                                  |
//! | `restored`              | the restored value (archived entries are frozen); archived by then, its TTL then lost |
//!
//! A `restored` first change does not say the entry was already archived
//! when the transaction ran: it may have expired in between, and expiry is
//! not a change. Its TTL then is lost, as for an entry archived now, and
//! `state` decides its liveness the same way (see [`Past::restored_since`]).
//!
//! An entry last written in the transaction's own ledger with no change after
//! the transaction was written *before* it (by an earlier transaction, or by
//! fee processing): the current value is the one the transaction saw.
//!
//! A TTL lives in a separate `Ttl` entry keyed by the hash of the entry's key,
//! and changes the same way, so the first later `state` of that entry is the
//! exact `liveUntilLedgerSeq` the transaction saw.
//!
//! # Where the first later change is looked for
//!
//! - **Forward**, from the transaction's own ledger, [`FORWARD_LEDGERS`]
//!   ledgers at most. This is what resolves busy contracts: the next write is
//!   a few ledgers away. Every footprint key and TTL is watched here, not only
//!   the drifted ones, so TTL extensions and deletions in that window are
//!   rebuilt as well. The scan runs whenever current state cannot rule such
//!   changes out (see [`rebuild`]), and stops early once every drifted entry
//!   and every watched TTL or missing entry has its first later change, or at
//!   the RPC's latest ledger. Its pages are requested together, so the window
//!   costs about one round trip.
//! - **Backward**, for entries not written again that soon: the entry's
//!   current `lastModifiedLedgerSeq` names the ledger of its *last* change,
//!   whose `state` record names the ledger of the one before, and so on. The
//!   chain is followed until a state last modified at or before the
//!   transaction's ledger turns up, [`BACKWARD_HOPS`] ledgers at most. Only a
//!   `state` record continues the chain; a `created` or `restored` one says
//!   nothing about what came before it.
//!
//! # Limits
//!
//! - Metadata is only served inside the RPC's retention window (about seven
//!   days by default). Past it, nothing can be rebuilt.
//! - Each ledger is 100 KB to a few MB of metadata. It is not fetched when
//!   current state proves nothing changed: no drifted entry, no missing one,
//!   and no TTL that could still have been extended (a TTL that ran out
//!   before the transaction can only move by a restoration, which rewrites
//!   the entry). In practice that spares classic-only footprints. Fixtures
//!   keep only the changes to the keys they recorded (`sororewindChanges`,
//!   see `transport`).
//! - Changes that leave no trace in current state are only seen inside the
//!   forward window: a TTL extension does not touch the entry's
//!   `lastModifiedLedgerSeq`, and a deleted entry is simply absent.
//! - Evictions are not transaction changes and are not followed.
//! - Network settings are not rebuilt.
//!
//! An entry that cannot be rebuilt is replayed as it is now and stays in the
//! report's drifted list, with the reason.

use std::collections::{HashMap, HashSet};

use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use stellar_xdr::{
    Hash, LedgerCloseMeta, LedgerEntry, LedgerEntryChange, LedgerEntryChanges, LedgerEntryData,
    LedgerKey, LedgerKeyTtl, Limits, TransactionMeta, WriteXdr,
};

use crate::codec;
use crate::error::{Result, RewindError};
use crate::rpc::{RawChangeGroup, RawLedger, RawLedgerEntry, RpcClient};

/// How many ledgers, starting with the transaction's own, are scanned for the
/// next change to each drifted or watched entry (see [`rebuild`]).
pub const FORWARD_LEDGERS: u32 = 16;

/// Ledgers fetched per `getLedgers` call while scanning forward.
const FORWARD_PAGE: u32 = 4;

/// How many ledgers the backward walk may fetch in total.
pub const BACKWARD_HOPS: u32 = 24;

/// The transaction whose view of the ledger is being rebuilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// The ledger it was applied in.
    pub ledger: u32,
    /// One-based position in the ledger's apply order.
    pub application_order: Option<u32>,
}

/// An entry as the transaction saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PastValue {
    /// Its value then, from the `state` record of the first later change.
    Entry(Box<LedgerEntry>),
    /// It did not exist yet.
    Absent,
    /// The entry on the ledger now: its last change was made in the
    /// transaction's own ledger, but before the transaction ran.
    Unchanged,
}

/// What the metadata says about one footprint key; `None` where it says
/// nothing and the current state stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Past {
    /// The entry's value when the transaction ran.
    pub value: Option<PastValue>,
    /// The entry's `liveUntilLedgerSeq` when the transaction ran.
    pub live_until: Option<u32>,
    /// The entry was restored from the archive after the transaction, before
    /// any other change to it: it was archived by then, and unless
    /// `live_until` is known, the TTL it had when the transaction ran is lost
    /// just as for an entry archived now.
    pub restored_since: bool,
}

/// The outcome of a rebuild: one [`Past`] per footprint key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rebuild {
    /// In footprint order.
    pub past: Vec<Past>,
    /// Why some drifted entries were left as they are now.
    pub note: Option<String>,
}

/// When, within a ledger, a group of changes is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    /// `feeProcessing`: the fee charged before any transaction is applied.
    Fee,
    /// `txChangesBefore`: the sequence-number bump.
    Before,
    /// The operations' changes.
    Operations,
    /// `txChangesAfter`.
    After,
    /// `postTxApplyFeeProcessing`: Soroban refunds.
    PostFee,
    /// `upgradesProcessing`.
    Upgrade,
}

/// The changes one transaction (or upgrade) made in one phase.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Group {
    phase: Phase,
    /// Zero-based apply position; for upgrades, the upgrade's position.
    tx: u32,
    changes: Vec<LedgerEntryChange>,
}

impl Group {
    /// Sort key reproducing the order described in the module docs.
    fn order(&self) -> (u8, u32, Phase) {
        let stage = match self.phase {
            Phase::Fee => 0,
            Phase::Before | Phase::Operations | Phase::After => 1,
            Phase::PostFee => 2,
            Phase::Upgrade => 3,
        };
        (stage, self.tx, self.phase)
    }
}

/// Every ledger entry change of one ledger, in the order it was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerChanges {
    groups: Vec<Group>,
}

impl LedgerChanges {
    /// Read the changes from a `getLedgers` record: the full `metadataXdr`
    /// of a live RPC, or the `sororewindChanges` a fixture keeps of it.
    pub fn from_raw(raw: &RawLedger) -> Result<Self> {
        let mut groups = match (&raw.changes, &raw.metadata_xdr) {
            (Some(groups), _) => groups
                .iter()
                .map(|group| {
                    let changes: LedgerEntryChanges =
                        codec::decode(&group.changes_xdr, "sororewindChanges")?;
                    Ok(Group {
                        phase: group.phase,
                        tx: group.tx,
                        changes: changes.0.into(),
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            (None, Some(metadata)) => groups_of(&codec::decode(metadata, "metadataXdr")?),
            (None, None) => {
                return Err(RewindError::MissingField {
                    method: "getLedgers",
                    field: "metadataXdr",
                });
            }
        };
        groups.sort_by_key(Group::order);
        Ok(Self { groups })
    }

    /// The changes applied after the host ran for the transaction at
    /// `position`; all of them when there is no position.
    fn after(&self, position: Option<u32>) -> impl Iterator<Item = &LedgerEntryChange> {
        self.groups
            .iter()
            .filter(move |group| match position {
                None => true,
                Some(tx) => group.order() > (1, tx, Phase::Before),
            })
            .flat_map(|group| group.changes.iter())
    }
}

fn groups_of(meta: &LedgerCloseMeta) -> Vec<Group> {
    let mut groups = Vec::new();
    let mut push = |phase: Phase, tx: usize, changes: &LedgerEntryChanges| {
        if !changes.is_empty() {
            groups.push(Group {
                phase,
                tx: u32::try_from(tx).unwrap_or(u32::MAX),
                changes: changes.to_vec(),
            });
        }
    };
    let apply = |tx: usize,
                 meta: &TransactionMeta,
                 push: &mut dyn FnMut(Phase, usize, &LedgerEntryChanges)| {
        match meta {
            TransactionMeta::V0(operations) => {
                for operation in operations.iter() {
                    push(Phase::Operations, tx, &operation.changes);
                }
            }
            TransactionMeta::V1(v1) => {
                push(Phase::Before, tx, &v1.tx_changes);
                for operation in v1.operations.iter() {
                    push(Phase::Operations, tx, &operation.changes);
                }
            }
            TransactionMeta::V2(v2) => {
                push(Phase::Before, tx, &v2.tx_changes_before);
                for operation in v2.operations.iter() {
                    push(Phase::Operations, tx, &operation.changes);
                }
                push(Phase::After, tx, &v2.tx_changes_after);
            }
            TransactionMeta::V3(v3) => {
                push(Phase::Before, tx, &v3.tx_changes_before);
                for operation in v3.operations.iter() {
                    push(Phase::Operations, tx, &operation.changes);
                }
                push(Phase::After, tx, &v3.tx_changes_after);
            }
            TransactionMeta::V4(v4) => {
                push(Phase::Before, tx, &v4.tx_changes_before);
                for operation in v4.operations.iter() {
                    push(Phase::Operations, tx, &operation.changes);
                }
                push(Phase::After, tx, &v4.tx_changes_after);
            }
        }
    };

    let upgrades = match meta {
        LedgerCloseMeta::V0(v0) => {
            for (tx, processing) in v0.tx_processing.iter().enumerate() {
                push(Phase::Fee, tx, &processing.fee_processing);
                apply(tx, &processing.tx_apply_processing, &mut push);
            }
            &v0.upgrades_processing
        }
        LedgerCloseMeta::V1(v1) => {
            for (tx, processing) in v1.tx_processing.iter().enumerate() {
                push(Phase::Fee, tx, &processing.fee_processing);
                apply(tx, &processing.tx_apply_processing, &mut push);
            }
            &v1.upgrades_processing
        }
        LedgerCloseMeta::V2(v2) => {
            for (tx, processing) in v2.tx_processing.iter().enumerate() {
                push(Phase::Fee, tx, &processing.fee_processing);
                apply(tx, &processing.tx_apply_processing, &mut push);
                push(Phase::PostFee, tx, &processing.post_tx_apply_fee_processing);
            }
            &v2.upgrades_processing
        }
    };
    for (index, upgrade) in upgrades.iter().enumerate() {
        push(Phase::Upgrade, index, &upgrade.changes);
    }
    groups
}

/// The key a change is about, XDR-encoded.
fn change_key(change: &LedgerEntryChange) -> Option<Vec<u8>> {
    let key = match change {
        LedgerEntryChange::Created(entry)
        | LedgerEntryChange::Updated(entry)
        | LedgerEntryChange::State(entry)
        | LedgerEntryChange::Restored(entry) => entry.to_key(),
        LedgerEntryChange::Removed(key) => key.clone(),
    };
    key.to_xdr(Limits::none()).ok()
}

/// The key of the `Ttl` entry that tracks the entry behind `key_xdr`.
pub fn ttl_key(key_xdr: &[u8]) -> Option<Vec<u8>> {
    LedgerKey::Ttl(LedgerKeyTtl {
        key_hash: Hash(Sha256::digest(key_xdr).into()),
    })
    .to_xdr(Limits::none())
    .ok()
}

/// Reduce a ledger's `metadataXdr` to the changes that touch `keep` (encoded
/// ledger keys), in the form fixtures store. `None` when it does not decode.
///
/// Only the records a rebuild reads are kept: `state`, `created` and
/// `restored`. The `updated` and `removed` records that follow a `state` hold
/// the entry *after* the change, which is never the past, and dropping them
/// halves the fixture.
pub fn slim(metadata_b64: &str, keep: &HashSet<Vec<u8>>) -> Option<Vec<RawChangeGroup>> {
    let meta: LedgerCloseMeta = codec::decode(metadata_b64, "metadataXdr").ok()?;
    let mut groups = groups_of(&meta);
    groups.sort_by_key(Group::order);
    Some(
        groups
            .into_iter()
            .filter_map(|group| {
                let changes: Vec<LedgerEntryChange> = group
                    .changes
                    .into_iter()
                    .filter(|change| {
                        !matches!(
                            change,
                            LedgerEntryChange::Updated(_) | LedgerEntryChange::Removed(_)
                        )
                    })
                    .filter(|change| change_key(change).is_some_and(|key| keep.contains(&key)))
                    .collect();
                if changes.is_empty() {
                    return None;
                }
                let changes = LedgerEntryChanges(changes.try_into().ok()?);
                Some(RawChangeGroup {
                    phase: group.phase,
                    tx: group.tx,
                    changes_xdr: codec::b64_encode(&changes.to_xdr(Limits::none()).ok()?),
                })
            })
            .collect(),
    )
}

/// What the first change after the transaction says about an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Found {
    Value(PastValue),
    /// A change that cannot come first (`updated` or `removed` without a
    /// `state`): the metadata is not what this module understands.
    Unusable,
}

/// The first-later-change bookkeeping for every footprint key.
struct Tracker {
    /// Encoded data key → footprint index.
    data: HashMap<Vec<u8>, usize>,
    /// Encoded TTL key → footprint index.
    ttl: HashMap<Vec<u8>, usize>,
    values: Vec<Option<Found>>,
    live_until: Vec<Option<u32>>,
    /// The TTL's first later change has been seen, whatever it was.
    ttl_seen: Vec<bool>,
    /// The first later change to the entry or its TTL was a restoration.
    restored: Vec<bool>,
}

impl Tracker {
    fn new(key_xdr: &[Vec<u8>]) -> Self {
        let mut data = HashMap::new();
        let mut ttl = HashMap::new();
        for (index, key) in key_xdr.iter().enumerate().rev() {
            data.insert(key.clone(), index);
            if let Some(ttl_key) = ttl_key(key) {
                ttl.insert(ttl_key, index);
            }
        }
        Self {
            data,
            ttl,
            values: vec![None; key_xdr.len()],
            live_until: vec![None; key_xdr.len()],
            ttl_seen: vec![false; key_xdr.len()],
            restored: vec![false; key_xdr.len()],
        }
    }

    /// Take in the changes of the next ledger, oldest first.
    fn observe<'a>(&mut self, changes: impl Iterator<Item = &'a LedgerEntryChange>) {
        for change in changes {
            let Some(key) = change_key(change) else {
                continue;
            };
            if let Some(&index) = self.data.get(&key) {
                if self.values[index].is_none() {
                    self.values[index] = Some(match change {
                        LedgerEntryChange::State(entry) => {
                            Found::Value(PastValue::Entry(Box::new(entry.clone())))
                        }
                        LedgerEntryChange::Restored(entry) => {
                            self.restored[index] = true;
                            Found::Value(PastValue::Entry(Box::new(entry.clone())))
                        }
                        LedgerEntryChange::Created(_) => Found::Value(PastValue::Absent),
                        LedgerEntryChange::Updated(_) | LedgerEntryChange::Removed(_) => {
                            Found::Unusable
                        }
                    });
                }
            } else if let Some(&index) = self.ttl.get(&key)
                && !self.ttl_seen[index]
            {
                self.ttl_seen[index] = true;
                match change {
                    LedgerEntryChange::State(LedgerEntry {
                        data: LedgerEntryData::Ttl(ttl),
                        ..
                    }) => self.live_until[index] = Some(ttl.live_until_ledger_seq),
                    // It holds the TTL the restoration set, not the old one.
                    LedgerEntryChange::Restored(_) => self.restored[index] = true,
                    _ => {}
                }
            }
        }
    }
}

/// The first change to `key` in a whole ledger.
fn first_change<'a>(changes: &'a LedgerChanges, key: &[u8]) -> Option<&'a LedgerEntryChange> {
    changes
        .after(None)
        .find(|change| change_key(change).is_some_and(|k| k == key))
}

/// Why an entry was left as it is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Unresolved {
    NoApplyOrder,
    /// The RPC did not serve the metadata of this ledger.
    Unavailable(u32),
    TooManyChanges,
    Unreadable,
}

impl Unresolved {
    fn describe(self) -> String {
        match self {
            Unresolved::NoApplyOrder => {
                "the RPC did not report the transaction's position in its ledger".to_string()
            }
            Unresolved::Unavailable(sequence) => format!(
                "the RPC does not serve the metadata of ledger {sequence} (it is outside the \
                 retention window, or this RPC keeps no metadata)"
            ),
            Unresolved::TooManyChanges => format!(
                "their history goes back more than {BACKWARD_HOPS} changes from the current state"
            ),
            Unresolved::Unreadable => {
                "the ledger metadata records their changes in a way that is not understood"
                    .to_string()
            }
        }
    }
}

/// Rebuild what the footprint looked like to the transaction. Never fails:
/// what cannot be rebuilt is left alone and explained in the note.
///
/// `current` holds the entries as `getLedgerEntries` returned them, one per
/// key of `key_xdr`.
///
/// Metadata is fetched unless current state proves nothing changed since the
/// transaction: an entry modified since is *drifted*, and a change that
/// leaves no trace in `lastModifiedLedgerSeq` is *watched* for in the forward
/// window wherever it cannot be ruled out. Only drifted entries that cannot
/// be rebuilt are explained in the note; a watched change that cannot be
/// looked for leaves the current state standing, as before the scan.
pub async fn rebuild(
    rpc: &RpcClient,
    target: &Target,
    key_xdr: &[Vec<u8>],
    current: &[Option<RawLedgerEntry>],
) -> Rebuild {
    let mut rebuild = Rebuild {
        past: vec![Past::default(); key_xdr.len()],
        note: None,
    };
    let raw = |index: usize| current.get(index).and_then(Option::as_ref);
    let last_modified =
        |index: usize| -> Option<u32> { raw(index).and_then(|raw| raw.last_modified_ledger_seq) };
    let drifted: Vec<usize> = (0..key_xdr.len())
        .filter(|&index| last_modified(index).is_some_and(|m| m >= target.ledger))
        .collect();
    // Changes that leave no trace in current state.
    let watched: Vec<usize> = (0..key_xdr.len())
        .filter(|&index| match raw(index) {
            // Deleted since, perhaps: a deletion leaves nothing behind.
            None => true,
            // A TTL extension does not touch the entry. A TTL that ran out
            // before the transaction cannot have moved since but by a
            // restoration, which rewrites the entry and shows as drift; `0`
            // (archived now) may hide anything.
            Some(raw) => raw
                .live_until_ledger_seq
                .is_some_and(|live_until| live_until == 0 || live_until >= target.ledger),
        })
        .collect();
    if drifted.is_empty() && watched.is_empty() {
        return rebuild;
    }

    let mut unresolved: Vec<Unresolved> = Vec::new();
    let Some(position) = target
        .application_order
        .and_then(|order| order.checked_sub(1))
    else {
        rebuild.note = note(drifted.len(), &[Unresolved::NoApplyOrder]);
        return rebuild;
    };

    // Forward: the ledgers right after the transaction, in order. Every page
    // is requested at once and read in order: the scan rarely stops early
    // (a watched TTL seldom changes), so pages one after another would cost
    // a round trip each for nothing. Those past an early stop are dropped.
    let mut tracker = Tracker::new(key_xdr);
    let mut next = target.ledger;
    let window_end = target.ledger.saturating_add(FORWARD_LEDGERS);
    let starts: Vec<u32> = (target.ledger..window_end)
        .step_by(FORWARD_PAGE as usize)
        .collect();
    let in_flight = starts.len();
    let mut pages = stream::iter(starts)
        .map(|start| async move {
            let limit = FORWARD_PAGE.min(window_end - start);
            (limit, rpc.get_ledgers(start, limit).await)
        })
        .buffered(in_flight);
    // Why the transaction's own ledger could not be read, when it could not.
    let mut own_ledger_failure = None;
    'forward: while next < window_end {
        // A drifted entry is settled by its first later change, or once its
        // last change lies behind us; a watched one only by its first later
        // change (of the entry when it is missing now, else of its TTL).
        let drift_pending = drifted.iter().any(|&index| {
            tracker.values[index].is_none() && last_modified(index).is_some_and(|m| m >= next)
        });
        let watch_pending = watched.iter().any(|&index| match raw(index) {
            None => tracker.values[index].is_none(),
            Some(_) => !tracker.ttl_seen[index],
        });
        if !drift_pending && !watch_pending {
            break;
        }
        // Pages are whole and in order, so this one starts at `next`.
        let Some((limit, ledgers)) = pages.next().await else {
            break;
        };
        let ledgers = match ledgers {
            Ok(ledgers) => ledgers,
            Err(err) => {
                log::debug!(target: "replay", "ledger metadata from {next}: {err}");
                if next == target.ledger {
                    own_ledger_failure = Some(Unresolved::Unavailable(next));
                }
                break;
            }
        };
        let page = next..next + limit;
        for expected in page {
            let Some(raw) = ledgers.iter().find(|ledger| ledger.sequence == expected) else {
                // Past the RPC's latest ledger, there is nothing further to
                // scan: a page that stops short, or an empty page after a
                // full one. Only the transaction's own ledger must be there.
                if expected == target.ledger {
                    own_ledger_failure = Some(Unresolved::Unavailable(expected));
                }
                break 'forward;
            };
            match LedgerChanges::from_raw(raw) {
                Ok(changes) => {
                    tracker.observe(changes.after((expected == target.ledger).then_some(position)))
                }
                Err(err) => {
                    log::debug!(target: "replay", "metadata of ledger {expected}: {err}");
                    if expected == target.ledger {
                        own_ledger_failure = Some(Unresolved::Unavailable(expected));
                    }
                    break 'forward;
                }
            }
            next = expected + 1;
        }
    }
    // Cancel the pages the scan stopped short of.
    drop(pages);
    // `next` is the first ledger the forward scan did not cover.
    let covered_until = next;

    for (index, past) in rebuild.past.iter_mut().enumerate() {
        match &tracker.values[index] {
            Some(Found::Value(value)) => past.value = Some(value.clone()),
            Some(Found::Unusable) if drifted.contains(&index) => {
                unresolved.push(Unresolved::Unreadable);
            }
            Some(Found::Unusable) | None => {}
        }
        // A TTL only stands on its own when the value it belongs to is known:
        // rebuilt, or untouched since the transaction.
        if tracker.values[index] != Some(Found::Unusable) {
            past.live_until = tracker.live_until[index];
            past.restored_since = tracker.restored[index];
        }
    }

    // Backward: follow each remaining entry's chain of `lastModified` ledgers.
    let mut fetched: HashMap<u32, Option<LedgerChanges>> = HashMap::new();
    let mut hops = 0u32;
    for &index in &drifted {
        if tracker.values[index].is_some() {
            continue;
        }
        if covered_until == target.ledger {
            // Not even the transaction's own ledger could be read, so changes
            // made in it after the transaction cannot be told from earlier ones.
            unresolved.push(own_ledger_failure.unwrap_or(Unresolved::Unavailable(target.ledger)));
            continue;
        }
        let mut at = match last_modified(index) {
            Some(at) => at,
            None => continue,
        };
        if at == target.ledger {
            // Written in the transaction's own ledger, and the forward scan
            // saw no change after the transaction there: it was written
            // before it, and is what the transaction saw.
            rebuild.past[index].value = Some(PastValue::Unchanged);
            continue;
        }
        let outcome = loop {
            if at < covered_until {
                // The forward scan read that ledger and saw no change after
                // the transaction: the metadata contradicts the entry.
                log::debug!(
                    target: "replay",
                    "footprint entry {index} was last modified in ledger {at}, where no change \
                     to it follows the transaction"
                );
                break Err(Unresolved::Unreadable);
            }
            #[allow(clippy::map_entry)] // the fetch in between awaits and may bail out
            if !fetched.contains_key(&at) {
                if hops >= BACKWARD_HOPS {
                    break Err(Unresolved::TooManyChanges);
                }
                hops += 1;
                let changes = match rpc.get_ledgers(at, 1).await {
                    Ok(ledgers) => ledgers
                        .iter()
                        .find(|ledger| ledger.sequence == at)
                        .and_then(|raw| LedgerChanges::from_raw(raw).ok()),
                    Err(err) => {
                        log::debug!(target: "replay", "ledger metadata of {at}: {err}");
                        None
                    }
                };
                fetched.insert(at, changes);
            }
            let Some(Some(changes)) = fetched.get(&at) else {
                break Err(Unresolved::Unavailable(at));
            };
            match first_change(changes, &key_xdr[index]) {
                Some(LedgerEntryChange::State(entry)) => {
                    let previous = entry.last_modified_ledger_seq;
                    if previous <= target.ledger {
                        // Written before the transaction (in an earlier
                        // ledger, or earlier in its own: the forward scan saw
                        // nothing after it there) and not again until `at`.
                        break Ok(entry.clone());
                    }
                    if previous >= at {
                        break Err(Unresolved::Unreadable);
                    }
                    at = previous;
                }
                // `created` and `restored` say nothing about what came before.
                other => {
                    log::debug!(
                        target: "replay",
                        "ledger {at}: the first change to footprint entry {index} is {}",
                        other.map_or("missing", LedgerEntryChange::name)
                    );
                    break Err(Unresolved::Unreadable);
                }
            }
        };
        match outcome {
            Ok(entry) => rebuild.past[index].value = Some(PastValue::Entry(Box::new(entry))),
            Err(reason) => unresolved.push(reason),
        }
    }

    rebuild.note = note(unresolved.len(), &unresolved);
    rebuild
}

fn note(entries: usize, reasons: &[Unresolved]) -> Option<String> {
    let mut reasons = reasons.to_vec();
    reasons.sort();
    reasons.dedup();
    if entries == 0 || reasons.is_empty() {
        return None;
    }
    let reasons: Vec<String> = reasons.iter().map(|reason| reason.describe()).collect();
    Some(format!(
        "{entries} drifted {} could not be rebuilt from ledger metadata: {}",
        if entries == 1 { "entry" } else { "entries" },
        reasons.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MemoryTransport, account_entry, data_entry, raw_entry};
    use serde_json::{Value, json};
    use stellar_xdr::{
        ContractDataDurability::Persistent, LedgerCloseMetaV2, OperationMetaV2, TransactionMetaV4,
        TransactionResultMetaV1, TtlEntry,
    };

    const N: u32 = 1_000;
    /// The transaction is the fourth of its ledger.
    const TARGET: Target = Target {
        ledger: N,
        application_order: Some(4),
    };

    type Groups = Vec<(Phase, u32, Vec<LedgerEntryChange>)>;

    fn key_of(entry: &LedgerEntry) -> Vec<u8> {
        entry.to_key().to_xdr(Limits::none()).unwrap()
    }

    fn at(entry: &LedgerEntry, last_modified: u32) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: last_modified,
            ..entry.clone()
        }
    }

    fn update(before: &LedgerEntry, after: &LedgerEntry) -> Vec<LedgerEntryChange> {
        vec![
            LedgerEntryChange::State(before.clone()),
            LedgerEntryChange::Updated(after.clone()),
        ]
    }

    fn ttl_entry(of: &LedgerEntry, live_until: u32) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Ttl(TtlEntry {
                key_hash: Hash(Sha256::digest(key_of(of)).into()),
                live_until_ledger_seq: live_until,
            }),
            ext: stellar_xdr::LedgerEntryExt::V0,
        }
    }

    /// An RPC that serves the given ledgers' changes, in the form fixtures
    /// keep them, and nothing past `latest`.
    fn rpc(ledgers: Vec<(u32, Groups)>, latest: u32) -> (RpcClient, crate::test_support::Calls) {
        let transport = serving(ledgers, latest);
        let calls = transport.calls();
        (RpcClient::new(transport), calls)
    }

    fn serving(ledgers: Vec<(u32, Groups)>, latest: u32) -> MemoryTransport {
        MemoryTransport::new().on_fn("getLedgers", move |params| {
            let start = params["startLedger"].as_u64().unwrap() as u32;
            let limit = params["pagination"]["limit"].as_u64().unwrap() as u32;
            let served: Vec<Value> = (start..start + limit)
                .filter(|sequence| *sequence <= latest)
                .map(|sequence| {
                    let groups: Vec<Value> = ledgers
                        .iter()
                        .filter(|(at, _)| *at == sequence)
                        .flat_map(|(_, groups)| groups)
                        .map(|(phase, tx, changes)| {
                            let changes = LedgerEntryChanges(changes.clone().try_into().unwrap());
                            json!({
                                "phase": phase,
                                "tx": tx,
                                "changesXdr":
                                    codec::b64_encode(&changes.to_xdr(Limits::none()).unwrap()),
                            })
                        })
                        .collect();
                    json!({"sequence": sequence, "headerXdr": "", "sororewindChanges": groups})
                })
                .collect();
            json!({"result": {"ledgers": served}})
        })
    }

    async fn rebuild_one(
        rpc: &RpcClient,
        now: &LedgerEntry,
        live_until: Option<u32>,
    ) -> (Past, Option<String>) {
        let rebuilt = rebuild(
            rpc,
            &TARGET,
            &[key_of(now)],
            &[Some(raw_entry(now, live_until))],
        )
        .await;
        (rebuilt.past[0].clone(), rebuilt.note)
    }

    #[tokio::test]
    async fn nothing_is_fetched_when_nothing_drifted() {
        let (rpc, calls) = rpc(Vec::new(), N + 100);
        let entry = account_entry([1; 32], N - 1);
        let (past, note) = rebuild_one(&rpc, &entry, None).await;
        assert_eq!((past, note), (Past::default(), None));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_state_before_the_next_change_is_the_past_value() {
        let now = account_entry([1; 32], N + 2);
        let (v0, v1, v2) = (at(&now, N - 9), at(&now, N), at(&now, N + 2));
        let ledgers = vec![
            (
                N,
                vec![
                    // Fees are charged before anything is applied, and the
                    // third transaction ran before ours: neither is "after".
                    (Phase::Fee, 7, update(&v0, &v0)),
                    (Phase::Operations, 2, update(&v0, &v1)),
                    // Our own sequence bump precedes the host too.
                    (Phase::Before, 3, update(&v1, &v1)),
                ],
            ),
            (N + 2, vec![(Phase::Operations, 0, update(&v1, &v2))]),
        ];
        let (rpc, _) = rpc(ledgers, N + 100);
        let (past, note) = rebuild_one(&rpc, &now, None).await;
        assert_eq!(past.value, Some(PastValue::Entry(Box::new(v1))));
        assert_eq!(note, None);
    }

    #[tokio::test]
    async fn a_later_transaction_of_the_same_ledger_counts() {
        let now = account_entry([1; 32], N);
        let (seen, after) = (at(&now, N - 9), at(&now, N));
        for group in [
            (Phase::Operations, 3, update(&seen, &after)),
            (Phase::After, 3, update(&seen, &after)),
            (Phase::Before, 4, update(&seen, &after)),
            (Phase::PostFee, 0, update(&seen, &after)),
            (Phase::Upgrade, 0, update(&seen, &after)),
        ] {
            let (rpc, _) = rpc(vec![(N, vec![group.clone()])], N + 100);
            let (past, _) = rebuild_one(&rpc, &now, None).await;
            assert_eq!(
                past.value,
                Some(PastValue::Entry(Box::new(seen.clone()))),
                "{group:?}"
            );
        }
    }

    #[tokio::test]
    async fn written_earlier_in_the_same_ledger_is_not_drift() {
        let now = account_entry([1; 32], N);
        let before = at(&now, N - 9);
        let ledgers = vec![(N, vec![(Phase::Operations, 1, update(&before, &now))])];
        let (rpc, _) = rpc(ledgers, N + 100);
        let (past, note) = rebuild_one(&rpc, &now, None).await;
        assert_eq!(past.value, Some(PastValue::Unchanged));
        assert_eq!(note, None);
    }

    #[tokio::test]
    async fn an_entry_created_later_did_not_exist() {
        let now = data_entry([2; 32], Persistent, N + 1);
        let ledgers = vec![(
            N + 1,
            vec![(
                Phase::Operations,
                0,
                vec![
                    LedgerEntryChange::Created(now.clone()),
                    LedgerEntryChange::Created(ttl_entry(&now, N + 500)),
                ],
            )],
        )];
        let (rpc, _) = rpc(ledgers, N + 100);
        let (past, note) = rebuild_one(&rpc, &now, Some(N + 500)).await;
        assert_eq!(past.value, Some(PastValue::Absent));
        assert_eq!(past.live_until, None);
        assert_eq!(note, None);
    }

    #[tokio::test]
    async fn the_ttl_is_rebuilt_from_its_own_entry() {
        let now = data_entry([2; 32], Persistent, N + 1);
        let then = at(&now, N - 5);
        let ledgers = vec![(
            N + 1,
            vec![
                (Phase::Operations, 0, update(&then, &now)),
                (
                    Phase::Operations,
                    0,
                    update(&ttl_entry(&now, N + 40), &ttl_entry(&now, N + 9_000)),
                ),
                // Only the first change counts.
                (
                    Phase::Operations,
                    1,
                    update(&ttl_entry(&now, N + 9_000), &ttl_entry(&now, N + 9_999)),
                ),
            ],
        )];
        let (rpc, _) = rpc(ledgers, N + 100);
        let (past, _) = rebuild_one(&rpc, &now, Some(N + 9_999)).await;
        assert_eq!(past.value, Some(PastValue::Entry(Box::new(then))));
        assert_eq!(past.live_until, Some(N + 40));
    }

    #[tokio::test]
    async fn a_restored_entry_keeps_its_value_but_not_its_ttl() {
        let now = data_entry([2; 32], Persistent, N + 3);
        let archived = at(&now, N - 700);
        let ledgers = vec![(
            N + 3,
            vec![(
                Phase::Operations,
                0,
                vec![
                    LedgerEntryChange::Restored(archived.clone()),
                    LedgerEntryChange::Restored(ttl_entry(&now, N + 500)),
                ],
            )],
        )];
        let (rpc, _) = rpc(ledgers, N + 100);
        let (past, _) = rebuild_one(&rpc, &now, Some(N + 500)).await;
        assert_eq!(past.value, Some(PastValue::Entry(Box::new(archived))));
        assert_eq!(past.live_until, None);
        // It was archived by then: its liveness is not the current TTL's.
        assert!(past.restored_since);
    }

    #[tokio::test]
    async fn a_ttl_extended_since_is_found_without_value_drift() {
        // The entry was not written since, but its TTL was extended: the
        // extension does not touch the entry's `lastModifiedLedgerSeq`.
        let now = data_entry([2; 32], Persistent, N - 5);
        let ledgers = vec![(
            N + 6,
            vec![(
                Phase::Operations,
                0,
                update(&ttl_entry(&now, N + 40), &ttl_entry(&now, N + 9_000)),
            )],
        )];
        let (rpc, _) = rpc(ledgers, N + 100);
        let (past, note) = rebuild_one(&rpc, &now, Some(N + 9_000)).await;
        assert_eq!(past.value, None);
        assert_eq!(past.live_until, Some(N + 40));
        assert_eq!(note, None);
    }

    /// A transport whose `getLedgers` calls are only answered once `pages`
    /// of them are waiting: a scan that awaited each page before asking for
    /// the next would wait forever.
    struct AllAtOnce {
        inner: MemoryTransport,
        gate: tokio::sync::Barrier,
    }

    impl crate::transport::Transport for AllAtOnce {
        fn request<'a>(
            &'a self,
            method: &'a str,
            params: &'a Value,
        ) -> crate::transport::BoxFuture<'a, Result<String>> {
            Box::pin(async move {
                if method == "getLedgers" {
                    self.gate.wait().await;
                }
                self.inner.request(method, params).await
            })
        }

        fn origin(&self) -> String {
            self.inner.origin()
        }
    }

    #[tokio::test]
    async fn the_forward_pages_are_requested_at_once() {
        // A watched TTL that does not move keeps the scan going to the end.
        let entry = data_entry([2; 32], Persistent, N - 9);
        let rpc = RpcClient::new(AllAtOnce {
            inner: serving(Vec::new(), N + 100),
            gate: tokio::sync::Barrier::new(FORWARD_LEDGERS.div_ceil(FORWARD_PAGE) as usize),
        });
        let rebuilt = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            rebuild_one(&rpc, &entry, Some(N + 50)),
        )
        .await
        .expect("the forward pages were requested one after another");
        assert_eq!(rebuilt, (Past::default(), None));
    }

    #[tokio::test]
    async fn an_entry_deleted_since_is_found_without_value_drift() {
        let then = data_entry([2; 32], Persistent, N - 5);
        let ledgers = vec![(
            N + 1,
            vec![(
                Phase::Operations,
                0,
                // `removed` records follow, but fixtures do not keep them.
                vec![
                    LedgerEntryChange::State(then.clone()),
                    LedgerEntryChange::State(ttl_entry(&then, N + 40)),
                ],
            )],
        )];
        let (rpc, _) = rpc(ledgers, N + 100);
        let rebuilt = rebuild(&rpc, &TARGET, &[key_of(&then)], &[None]).await;
        assert_eq!(
            rebuilt.past[0],
            Past {
                value: Some(PastValue::Entry(Box::new(then))),
                live_until: Some(N + 40),
                restored_since: false,
            }
        );
        assert_eq!(rebuilt.note, None);
    }

    #[tokio::test]
    async fn nothing_is_fetched_when_a_ttl_ran_out_before_the_transaction() {
        // Only a restoration could move it since, and that rewrites the entry.
        let (rpc, calls) = rpc(Vec::new(), N + 100);
        let entry = data_entry([2; 32], Persistent, N - 9);
        let (past, note) = rebuild_one(&rpc, &entry, Some(N - 1)).await;
        assert_eq!((past, note), (Past::default(), None));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_forward_scan_stops_at_the_latest_ledger() {
        let entry = data_entry([2; 32], Persistent, N - 9);
        let starts = |calls: crate::test_support::Calls| -> Vec<u64> {
            calls
                .lock()
                .unwrap()
                .iter()
                .map(|(_, params)| params["startLedger"].as_u64().unwrap())
                .collect()
        };
        let n = u64::from(N);
        // This RPC answers at once, so a page is only requested once the scan
        // reads on: the requests show where it stopped reading.

        // A page that stops short.
        let (short, calls) = rpc(Vec::new(), N + 5);
        let (past, note) = rebuild_one(&short, &entry, Some(N + 50)).await;
        assert_eq!((past, note), (Past::default(), None));
        assert_eq!(starts(calls), [n, n + 4]);

        // An empty page after a full one is the end, not a failure.
        let (empty, calls) = rpc(Vec::new(), N + 3);
        let (past, note) = rebuild_one(&empty, &entry, Some(N + 50)).await;
        assert_eq!((past, note), (Past::default(), None));
        assert_eq!(starts(calls), [n, n + 4]);

        // A drifted entry is still settled by its own last change.
        let now = account_entry([1; 32], N + 3);
        let ledgers = vec![(
            N + 3,
            vec![(Phase::Operations, 0, update(&at(&now, N - 2), &now))],
        )];
        let (rpc_drifted, _) = rpc(ledgers, N + 3);
        let (past, note) = rebuild_one(&rpc_drifted, &now, None).await;
        assert_eq!(
            past.value,
            Some(PastValue::Entry(Box::new(at(&now, N - 2))))
        );
        assert_eq!(note, None);

        // Only the transaction's own ledger must be served.
        let (behind, _) = rpc(Vec::new(), N - 1);
        let (past, note) = rebuild_one(&behind, &now, None).await;
        assert_eq!(past, Past::default());
        assert!(note.unwrap().contains(&format!("ledger {N} ")));
    }

    #[tokio::test]
    async fn the_chain_of_last_modified_ledgers_is_followed_backwards() {
        let now = account_entry([1; 32], N + 300);
        let (seen, middle) = (at(&now, N - 9), at(&now, N + 120));
        let ledgers = vec![
            (N + 120, vec![(Phase::Fee, 0, update(&seen, &middle))]),
            (N + 300, vec![(Phase::Operations, 0, update(&middle, &now))]),
        ];
        let (rpc, calls) = rpc(ledgers, N + 1_000);
        let (past, note) = rebuild_one(&rpc, &now, None).await;
        assert_eq!(past.value, Some(PastValue::Entry(Box::new(seen))));
        assert_eq!(note, None);

        let starts: Vec<u64> = calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, params)| params["startLedger"].as_u64().unwrap())
            .collect();
        // The forward window, then one ledger per hop.
        let (n, hops) = (u64::from(N), [u64::from(N + 300), u64::from(N + 120)]);
        assert_eq!(starts[..4], [n, n + 4, n + 8, n + 12]);
        assert_eq!(starts[4..], hops);
    }

    #[tokio::test]
    async fn what_cannot_be_rebuilt_is_left_alone_and_explained() {
        let now = account_entry([1; 32], N + 300);

        // The RPC has no metadata at all.
        let none = RpcClient::new(
            MemoryTransport::new().on("getLedgers", &json!({"error": {"message": "pruned"}})),
        );
        let (past, note) = rebuild_one(&none, &now, None).await;
        assert_eq!(past, Past::default());
        let note = note.unwrap();
        assert!(
            note.starts_with("1 drifted entry could not be rebuilt from ledger metadata")
                && note.contains(&format!("ledger {N}")),
            "{note}"
        );

        // The ledger of the last change is gone.
        let (gone, _) = rpc(Vec::new(), N + 200);
        let (past, note) = rebuild_one(&gone, &now, None).await;
        assert_eq!(past, Past::default());
        assert!(note.unwrap().contains(&format!("ledger {}", N + 300)));

        // A `created` record says nothing about what came before it.
        let created = vec![(
            N + 300,
            vec![(
                Phase::Operations,
                0,
                vec![LedgerEntryChange::Created(now.clone())],
            )],
        )];
        let (rpc_created, _) = rpc(created, N + 1_000);
        let (past, note) = rebuild_one(&rpc_created, &now, None).await;
        assert_eq!(past, Past::default());
        assert!(note.unwrap().contains("not understood"));

        // More hops than the walk allows.
        let chain: Vec<(u32, Groups)> = (1..=BACKWARD_HOPS + 1)
            .map(|hop| {
                let ledger = N + 100 * hop;
                let before = at(&now, ledger - 100);
                (
                    ledger,
                    vec![(Phase::Operations, 0, update(&before, &at(&now, ledger)))],
                )
            })
            .collect();
        let last = N + 100 * (BACKWARD_HOPS + 1);
        let (long, _) = rpc(chain, last + 10);
        let (past, note) = rebuild_one(&long, &at(&now, last), None).await;
        assert_eq!(past, Past::default());
        assert!(note.unwrap().contains("more than"));
    }

    #[tokio::test]
    async fn the_apply_position_is_required() {
        let now = account_entry([1; 32], N + 2);
        let (rpc, calls) = rpc(Vec::new(), N + 100);
        let target = Target {
            ledger: N,
            application_order: None,
        };
        let rebuilt = rebuild(
            &rpc,
            &target,
            &[key_of(&now)],
            &[Some(raw_entry(&now, None))],
        )
        .await;
        assert!(rebuilt.note.unwrap().contains("position in its ledger"));
        assert!(calls.lock().unwrap().is_empty());
    }

    /// A `LedgerCloseMeta` whose transactions each update `entry` once per
    /// phase, so the order of the groups can be read off the sequence numbers.
    fn close_meta(entry: &LedgerEntry, other: &LedgerEntry) -> LedgerCloseMeta {
        let step = |n: u32| -> LedgerEntryChanges {
            LedgerEntryChanges(
                vec![
                    LedgerEntryChange::State(at(entry, n)),
                    LedgerEntryChange::Updated(at(entry, n + 1)),
                    LedgerEntryChange::State(other.clone()),
                ]
                .try_into()
                .unwrap(),
            )
        };
        let tx = |base: u32| TransactionResultMetaV1 {
            fee_processing: step(base),
            tx_apply_processing: TransactionMeta::V4(TransactionMetaV4 {
                tx_changes_before: step(base + 10),
                operations: vec![OperationMetaV2 {
                    changes: step(base + 20),
                    ..OperationMetaV2::default()
                }]
                .try_into()
                .unwrap(),
                tx_changes_after: step(base + 30),
                ..TransactionMetaV4::default()
            }),
            post_tx_apply_fee_processing: step(base + 40),
            ..TransactionResultMetaV1::default()
        };
        LedgerCloseMeta::V2(LedgerCloseMetaV2 {
            tx_processing: vec![tx(100), tx(200)].try_into().unwrap(),
            ..LedgerCloseMetaV2::default()
        })
    }

    #[test]
    fn changes_are_ordered_the_way_core_applies_them() {
        let entry = account_entry([1; 32], 0);
        let other = account_entry([9; 32], 0);
        let meta = close_meta(&entry, &other);
        let raw = RawLedger {
            sequence: N,
            header_xdr: String::new(),
            metadata_xdr: Some(codec::b64_encode(&meta.to_xdr(Limits::none()).unwrap())),
            changes: None,
        };
        let key = key_of(&entry);
        let states = |changes: &LedgerChanges, position| -> Vec<u32> {
            changes
                .after(position)
                .filter_map(|change| match change {
                    LedgerEntryChange::State(e) if key_of(e) == key => {
                        Some(e.last_modified_ledger_seq)
                    }
                    _ => None,
                })
                .collect()
        };
        let changes = LedgerChanges::from_raw(&raw).unwrap();
        // All fees, then each transaction, then all refunds.
        assert_eq!(
            states(&changes, None),
            [100, 200, 110, 120, 130, 210, 220, 230, 140, 240]
        );
        // After the host ran for the first transaction...
        assert_eq!(
            states(&changes, Some(0)),
            [120, 130, 210, 220, 230, 140, 240]
        );
        // ...and for the second.
        assert_eq!(states(&changes, Some(1)), [220, 230, 140, 240]);

        // What a fixture keeps reads back the same, minus the other entries.
        let keep = HashSet::from([key.clone()]);
        let slimmed = RawLedger {
            metadata_xdr: None,
            changes: slim(raw.metadata_xdr.as_deref().unwrap(), &keep),
            ..raw.clone()
        };
        let kept = LedgerChanges::from_raw(&slimmed).unwrap();
        assert_eq!(states(&kept, Some(1)), [220, 230, 140, 240]);
        assert!(
            kept.after(None)
                .all(|change| change_key(change).as_ref() == Some(&key))
        );
        // One `state` per update: the `updated` halves are not kept.
        assert_eq!(kept.after(None).count(), 10);
        assert_eq!(slim("AAAA", &keep), None);

        // A record with neither form has no metadata to read.
        let bare = RawLedger {
            metadata_xdr: None,
            changes: None,
            ..raw
        };
        assert!(LedgerChanges::from_raw(&bare).is_err());
    }
}
