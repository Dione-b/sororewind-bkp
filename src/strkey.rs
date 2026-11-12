//! Strkey encodings of on-chain identifiers.
//!
//! `stellar-strkey` types expose an inherent `to_string` that returns a
//! fixed-capacity `heapless::String`; going through `format!` instead picks up
//! their `Display` impls and gives us an ordinary owned `String`.

use stellar_strkey::{ClaimableBalance, Contract, LiquidityPool, ed25519};
use stellar_xdr::{AccountId, ClaimableBalanceId, ContractId, Hash, PublicKey, ScAddress};

/// The `G…` strkey of an ed25519 public key.
pub fn account(bytes: &[u8; 32]) -> String {
    format!("{}", ed25519::PublicKey(*bytes))
}

/// The `G…` strkey of an account.
pub fn account_id(id: &AccountId) -> String {
    match &id.0 {
        PublicKey::PublicKeyTypeEd25519(key) => account(&key.0),
    }
}

/// The `C…` strkey of a contract id.
pub fn contract(bytes: &[u8; 32]) -> String {
    format!("{}", Contract(*bytes))
}

/// The `B…` strkey of a V0 claimable balance id.
pub fn claimable_balance(bytes: &[u8; 32]) -> String {
    format!("{}", ClaimableBalance::V0(*bytes))
}

/// The `L…` strkey of a liquidity pool id.
pub fn liquidity_pool(bytes: &[u8; 32]) -> String {
    format!("{}", LiquidityPool(*bytes))
}

/// Parse a `C…` strkey back into a contract id.
pub fn parse_contract(strkey: &str) -> Option<ContractId> {
    let contract = Contract::from_string(strkey).ok()?;
    Some(ContractId(Hash(contract.0)))
}

/// First character, an ellipsis, and the last `tail` characters: `C…XYZW`.
/// Enough to recognise an identifier without burning a line of width.
pub fn shorten(strkey: &str, tail: usize) -> String {
    let length = strkey.chars().count();
    if length <= tail + 2 {
        return strkey.to_string();
    }
    let first: String = strkey.chars().take(1).collect();
    let last: String = strkey.chars().skip(length - tail).collect();
    format!("{first}…{last}")
}

/// The strkey of any `ScAddress`; muxed addresses keep their mux id.
pub fn address(addr: &ScAddress) -> String {
    match addr {
        ScAddress::Account(id) => account_id(id),
        ScAddress::Contract(c) => contract(&c.0.0),
        // The `M…` strkey carries the mux id, so two sub-accounts of one
        // account stay distinct.
        ScAddress::MuxedAccount(m) => format!(
            "{}",
            ed25519::MuxedAccount {
                ed25519: m.ed25519.0,
                id: m.id,
            }
        ),
        ScAddress::ClaimableBalance(ClaimableBalanceId::ClaimableBalanceIdTypeV0(h)) => {
            claimable_balance(&h.0)
        }
        ScAddress::LiquidityPool(pool) => liquidity_pool(&pool.0.0),
        ScAddress::MuxedContract(m) => format!(
            "{}",
            stellar_strkey::MuxedContract {
                contract_id: m.contract_id.0.0,
                id: m.id,
            }
        ),
    }
}

/// [`address`], shortened to its first character and last four.
pub fn short_address(addr: &ScAddress) -> String {
    shorten(&address(addr), 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_only_long_strings() {
        assert_eq!(shorten("CABCDEFGHIJ", 4), "C…GHIJ");
        // Not only ASCII: characters are counted, never bytes.
        assert_eq!(shorten("éàèùçâêîôû", 3), "é…îôû");
        assert_eq!(shorten("CABCDE", 4), "CABCDE");
        assert_eq!(shorten("", 4), "");
    }

    #[test]
    fn encodes_by_kind() {
        assert!(account(&[0; 32]).starts_with('G'));
        assert!(contract(&[0; 32]).starts_with('C'));
        assert!(claimable_balance(&[0; 32]).starts_with('B'));
        assert!(liquidity_pool(&[0; 32]).starts_with('L'));
        assert_eq!(account(&[0; 32]).len(), 56);
    }

    #[test]
    fn contract_strkeys_round_trip() {
        let text = contract(&[9; 32]);
        assert_eq!(parse_contract(&text), Some(ContractId(Hash([9; 32]))));
        assert_eq!(parse_contract(&account(&[9; 32])), None);
        assert_eq!(parse_contract("nonsense"), None);
    }

    #[test]
    fn addresses_use_the_matching_strkey() {
        let c = ScAddress::Contract(ContractId(Hash([4; 32])));
        assert_eq!(address(&c), contract(&[4; 32]));
        assert_eq!(short_address(&c), shorten(&contract(&[4; 32]), 4));
        let a = ScAddress::Account(crate::test_support::account([5; 32]));
        assert_eq!(address(&a), account(&[5; 32]));
    }
}
