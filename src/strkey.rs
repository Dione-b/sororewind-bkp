//! Compact, human-readable encodings of on-chain identifiers.
//!
//! The full 56-character strkey is unreadable inline, so the narrative shows
//! `G…ABC` / `C…XYZ` and expands to the full value only where it matters.
//!
//! `stellar-strkey` types expose an inherent `to_string` that returns a
//! fixed-capacity `heapless::String`; going through `format!` instead picks up
//! their `Display` impls and gives us an ordinary owned `String`.

use stellar_strkey::{ed25519, ClaimableBalance, Contract, LiquidityPool};
use stellar_xdr::ScAddress;

pub fn account(bytes: &[u8; 32]) -> String {
    format!("{}", ed25519::PublicKey(*bytes))
}

pub fn contract(bytes: &[u8; 32]) -> String {
    format!("{}", Contract(*bytes))
}

/// `G…XYZ` — enough to recognise an account without burning a line of width.
pub fn short_account(bytes: &[u8; 32]) -> String {
    shorten(&account(bytes), 4)
}

/// `C…XYZ`
pub fn short_contract(bytes: &[u8; 32]) -> String {
    shorten(&contract(bytes), 4)
}

fn shorten(s: &str, tail: usize) -> String {
    if s.len() <= tail + 2 {
        return s.to_string();
    }
    format!("{}…{}", &s[..1], &s[s.len() - tail..])
}

pub fn address(addr: &ScAddress) -> String {
    match addr {
        ScAddress::Account(id) => account(&account_bytes(id)),
        ScAddress::Contract(c) => contract(&c.0 .0),
        ScAddress::MuxedAccount(m) => account(&m.ed25519.0),
        ScAddress::MuxedContract(m) => contract(&m.contract_id.0 .0),
        ScAddress::ClaimableBalance(id) => match id {
            stellar_xdr::ClaimableBalanceId::ClaimableBalanceIdTypeV0(h) => {
                format!("{}", ClaimableBalance::V0(h.0))
            }
        },
        ScAddress::LiquidityPool(pool) => format!("{}", LiquidityPool(pool.0 .0)),
    }
}

pub fn short_address(addr: &ScAddress) -> String {
    match addr {
        ScAddress::Account(id) => short_account(&account_bytes(id)),
        ScAddress::Contract(c) => short_contract(&c.0 .0),
        ScAddress::MuxedAccount(m) => short_account(&m.ed25519.0),
        ScAddress::MuxedContract(m) => short_contract(&m.contract_id.0 .0),
        other => address(other),
    }
}

fn account_bytes(id: &stellar_xdr::AccountId) -> [u8; 32] {
    match &id.0 {
        stellar_xdr::PublicKey::PublicKeyTypeEd25519(k) => k.0,
    }
}

/// One-line label for a footprint key. Drifted entries are printed with these,
/// so they have to be short enough to read in a list.
pub fn ledger_key(key: &stellar_xdr::LedgerKey) -> String {
    use stellar_xdr::LedgerKey;
    match key {
        LedgerKey::Account(k) => format!("account {}", account(&account_bytes(&k.account_id))),
        LedgerKey::Trustline(k) => {
            format!("trustline {}", account(&account_bytes(&k.account_id)))
        }
        LedgerKey::Offer(k) => format!(
            "offer {} of {}",
            k.offer_id,
            account(&account_bytes(&k.seller_id))
        ),
        LedgerKey::Data(k) => format!(
            "data {}/{}",
            account(&account_bytes(&k.account_id)),
            k.data_name.0
        ),
        LedgerKey::ClaimableBalance(k) => match &k.balance_id {
            stellar_xdr::ClaimableBalanceId::ClaimableBalanceIdTypeV0(h) => format!(
                "claimable balance {}",
                shorten(&format!("{}", ClaimableBalance::V0(h.0)), 8)
            ),
        },
        LedgerKey::LiquidityPool(k) => format!(
            "liquidity pool {}",
            shorten(&format!("{}", LiquidityPool(k.liquidity_pool_id.0 .0)), 8)
        ),
        LedgerKey::ContractData(k) => format!(
            "contract data {} ({})",
            short_address(&k.contract),
            k.durability
        ),
        LedgerKey::ContractCode(k) => format!("contract code {}", hex16(&k.hash.0)),
        LedgerKey::ConfigSetting(k) => format!("config setting {}", k.config_setting_id),
        LedgerKey::Ttl(k) => format!("ttl {}", hex16(&k.key_hash.0)),
    }
}

fn hex16(bytes: &[u8; 32]) -> String {
    format!("{}…", hex::encode(&bytes[..8]))
}
