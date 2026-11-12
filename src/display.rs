//! One home for turning XDR values into text.
//!
//! The narrative, the diagnostics listing, the drift report and the error
//! messages all describe the same values, so they all format them here. Each
//! concept has exactly one wording: an `ScError` reads the same in the
//! headline, inside an argument list and in the JSON document.

use std::fmt::{self, Write};

use stellar_xdr::{
    ClaimableBalanceId, ContractDataDurability, ContractEvent, ContractEventBody, DiagnosticEvent,
    InvokeHostFunctionResult, LedgerKey, ScError, ScErrorCode, ScErrorType, ScVal, TrustLineAsset,
};

use crate::strkey;

// --- errors -----------------------------------------------------------------

/// Lower-case name of an error category, as printed in `auth error …`.
pub fn error_type_name(error_type: ScErrorType) -> &'static str {
    match error_type {
        ScErrorType::Contract => "contract",
        ScErrorType::WasmVm => "wasm vm",
        ScErrorType::Context => "context",
        ScErrorType::Storage => "storage",
        ScErrorType::Object => "object",
        ScErrorType::Crypto => "crypto",
        ScErrorType::Events => "events",
        ScErrorType::Budget => "budget",
        ScErrorType::Value => "value",
        ScErrorType::Auth => "auth",
    }
}

/// The host-defined code of an error; `None` for contract errors, whose
/// number is the contract's own business.
pub fn error_code(error: &ScError) -> Option<ScErrorCode> {
    match error {
        ScError::Contract(_) => None,
        ScError::WasmVm(c)
        | ScError::Context(c)
        | ScError::Storage(c)
        | ScError::Object(c)
        | ScError::Crypto(c)
        | ScError::Events(c)
        | ScError::Budget(c)
        | ScError::Value(c)
        | ScError::Auth(c) => Some(*c),
    }
}

/// `contract error #3`, `auth error InvalidAction`, …
pub fn describe_error(error: &ScError) -> String {
    match error {
        ScError::Contract(n) => format!("contract error #{n}"),
        other => {
            let type_name = error_type_name(other.discriminant());
            match error_code(other) {
                Some(code) => format!("{type_name} error {}", code.name()),
                None => format!("{type_name} error"),
            }
        }
    }
}

// --- results ----------------------------------------------------------------

/// Stable machine name of a result code, used in the JSON document.
pub fn result_code(result: &InvokeHostFunctionResult) -> &'static str {
    match result {
        InvokeHostFunctionResult::Success(_) => "success",
        InvokeHostFunctionResult::Malformed => "malformed",
        InvokeHostFunctionResult::Trapped => "trapped",
        InvokeHostFunctionResult::ResourceLimitExceeded => "resource_limit_exceeded",
        InvokeHostFunctionResult::EntryArchived => "entry_archived",
        InvokeHostFunctionResult::InsufficientRefundableFee => "insufficient_refundable_fee",
    }
}

// --- values -----------------------------------------------------------------

/// Nesting beyond this is elided as `…`; diagnostics are meant to be skimmed.
const MAX_DEPTH: usize = 4;
/// Byte strings longer than this are abbreviated to a prefix and a length.
const MAX_INLINE_BYTES: usize = 16;
/// Vectors and maps show this many items, then how many more there are. The
/// JSON document carries every value whole, as XDR.
const MAX_ITEMS: usize = 8;

/// A one-line rendering of an `ScVal`.
pub fn scval(value: &ScVal) -> String {
    let mut out = String::new();
    // Writing into a `String` cannot fail.
    let _ = write_scval(value, &mut out, 0);
    out
}

fn write_scval(value: &ScVal, out: &mut String, depth: usize) -> fmt::Result {
    if depth > MAX_DEPTH {
        return out.write_char('…');
    }
    match value {
        ScVal::Void => out.write_str("void"),
        ScVal::Bool(b) => write!(out, "{b}"),
        ScVal::U32(n) => write!(out, "{n}"),
        ScVal::I32(n) => write!(out, "{n}"),
        ScVal::U64(n) => write!(out, "{n}"),
        ScVal::I64(n) => write!(out, "{n}"),
        ScVal::Timepoint(t) => write!(out, "{}", t.0),
        ScVal::Duration(d) => write!(out, "{}s", d.0),
        ScVal::U128(p) => write!(out, "{}", (u128::from(p.hi) << 64) | u128::from(p.lo)),
        ScVal::I128(p) => write!(out, "{}", (i128::from(p.hi) << 64) | i128::from(p.lo)),
        // 256-bit integers are printed in decimal like every other integer.
        ScVal::U256(p) => out.write_str(&decimal_256([p.hi_hi, p.hi_lo, p.lo_hi, p.lo_lo])),
        ScVal::I256(p) => {
            // Reinterpret the top limb's bits; negation below is two's complement.
            let limbs = [p.hi_hi as u64, p.hi_lo, p.lo_hi, p.lo_lo];
            if p.hi_hi < 0 {
                out.write_char('-')?;
                out.write_str(&decimal_256(negate_256(limbs)))
            } else {
                out.write_str(&decimal_256(limbs))
            }
        }
        ScVal::Bytes(b) => {
            if b.0.len() <= MAX_INLINE_BYTES {
                write!(out, "0x{}", hex::encode(&b.0))
            } else {
                write!(out, "0x{}…({} bytes)", hex::encode(&b.0[..8]), b.0.len())
            }
        }
        // `StringM`'s `Display` escapes backslashes and unprintable bytes but
        // not quotes; escape those too so the result is a string literal.
        ScVal::String(s) => write!(out, "\"{}\"", s.0.to_string().replace('"', "\\\"")),
        ScVal::Symbol(s) => write!(out, "{}", s.0),
        ScVal::Error(e) => out.write_str(&describe_error(e)),
        ScVal::Address(a) => out.write_str(&strkey::address(a)),
        ScVal::ContractInstance(_) => out.write_str("<contract instance>"),
        ScVal::LedgerKeyContractInstance => out.write_str("<contract instance key>"),
        ScVal::LedgerKeyNonce(n) => write!(out, "<nonce {}>", n.nonce),
        ScVal::ExecutableTag(s) => write!(out, "<executable {}>", s.0),
        ScVal::Vec(items) => {
            out.write_char('[')?;
            let items = items.as_ref().map_or(&[][..], |v| v.0.as_slice());
            for (i, item) in items.iter().take(MAX_ITEMS).enumerate() {
                if i > 0 {
                    out.write_str(", ")?;
                }
                write_scval(item, out, depth + 1)?;
            }
            write_elided(out, items.len())?;
            out.write_char(']')
        }
        ScVal::Map(entries) => {
            out.write_char('{')?;
            let entries = entries.as_ref().map_or(&[][..], |m| m.0.as_slice());
            for (i, entry) in entries.iter().take(MAX_ITEMS).enumerate() {
                if i > 0 {
                    out.write_str(", ")?;
                }
                write_scval(&entry.key, out, depth + 1)?;
                out.write_str(": ")?;
                write_scval(&entry.val, out, depth + 1)?;
            }
            write_elided(out, entries.len())?;
            out.write_char('}')
        }
    }
}

/// `, … 992 more` after the items of a collection that were shown.
fn write_elided(out: &mut String, total: usize) -> fmt::Result {
    if total > MAX_ITEMS {
        write!(out, ", … {} more", total - MAX_ITEMS)?;
    }
    Ok(())
}

/// Decimal digits of a 256-bit unsigned integer given as big-endian limbs.
fn decimal_256(mut limbs: [u64; 4]) -> String {
    let mut digits = Vec::new();
    loop {
        // Long division of the whole number by ten, limb by limb.
        let mut remainder = 0u128;
        for limb in &mut limbs {
            let current = (remainder << 64) | u128::from(*limb);
            *limb = (current / 10) as u64;
            remainder = current % 10;
        }
        digits.push(b'0' + remainder as u8);
        if limbs == [0; 4] {
            break;
        }
    }
    digits.iter().rev().map(|&d| d as char).collect()
}

/// Two's-complement negation of a 256-bit value given as big-endian limbs.
fn negate_256(limbs: [u64; 4]) -> [u64; 4] {
    let mut out = limbs.map(|limb| !limb);
    for limb in out.iter_mut().rev() {
        let (sum, carry) = limb.overflowing_add(1);
        *limb = sum;
        if !carry {
            break;
        }
    }
    out
}

// --- events -----------------------------------------------------------------

/// Topics and data of a contract event.
pub fn event_parts(event: &ContractEvent) -> (&[ScVal], &ScVal) {
    match &event.body {
        ContractEventBody::V0(body) => (body.topics.as_slice(), &body.data),
    }
}

/// One line per diagnostic event. On-chain and local events are rendered the
/// same way, so the two listings can be compared by eye or by `diff`.
pub fn diagnostic(event: &DiagnosticEvent) -> String {
    let contract = event
        .event
        .contract_id
        .as_ref()
        .map_or_else(|| "-".to_string(), |c| strkey::contract(&c.0.0));
    let (topics, data) = event_parts(&event.event);
    let topics: Vec<String> = topics.iter().map(scval).collect();
    format!(
        "{} [{contract}] {} {}",
        if event.in_successful_contract_call {
            "ok"
        } else {
            "fail"
        },
        topics.join(", "),
        scval(data)
    )
}

// --- ledger keys ------------------------------------------------------------

/// One-line label for a ledger key. Drifted and archived entries are listed
/// with these, so they have to be short enough to read in a list.
pub fn ledger_key(key: &LedgerKey) -> String {
    match key {
        LedgerKey::Account(k) => format!("account {}", strkey::account_id(&k.account_id)),
        LedgerKey::Trustline(k) => format!(
            "trustline {} {}",
            strkey::account_id(&k.account_id),
            trustline_asset(&k.asset)
        ),
        LedgerKey::Offer(k) => format!(
            "offer {} of {}",
            k.offer_id,
            strkey::account_id(&k.seller_id)
        ),
        LedgerKey::Data(k) => format!(
            "data {}/{}",
            strkey::account_id(&k.account_id),
            k.data_name.0
        ),
        LedgerKey::ClaimableBalance(k) => match &k.balance_id {
            ClaimableBalanceId::ClaimableBalanceIdTypeV0(h) => format!(
                "claimable balance {}",
                strkey::shorten(&strkey::claimable_balance(&h.0), 8)
            ),
        },
        LedgerKey::LiquidityPool(k) => format!(
            "liquidity pool {}",
            strkey::shorten(&strkey::liquidity_pool(&k.liquidity_pool_id.0.0), 8)
        ),
        LedgerKey::ContractData(k) => format!(
            "contract data {} {} ({})",
            strkey::short_address(&k.contract),
            scval(&k.key),
            match k.durability {
                ContractDataDurability::Temporary => "temporary",
                ContractDataDurability::Persistent => "persistent",
            }
        ),
        LedgerKey::ContractCode(k) => format!("contract code {}", hex_prefix(&k.hash.0)),
        LedgerKey::ConfigSetting(k) => format!("config setting {}", k.config_setting_id),
        LedgerKey::Ttl(k) => format!("ttl {}", hex_prefix(&k.key_hash.0)),
    }
}

/// `USDC:G…ABCD`, `native`, or `pool 1a2b…`: enough to tell two trustlines
/// of one account apart.
fn trustline_asset(asset: &TrustLineAsset) -> String {
    let credit = |code: &[u8], issuer: &stellar_xdr::AccountId| {
        let code = String::from_utf8_lossy(code);
        format!(
            "{}:{}",
            code.trim_end_matches('\0').escape_debug(),
            strkey::shorten(&strkey::account_id(issuer), 4)
        )
    };
    match asset {
        TrustLineAsset::Native => "native".to_string(),
        TrustLineAsset::CreditAlphanum4(a) => credit(&a.asset_code.0, &a.issuer),
        TrustLineAsset::CreditAlphanum12(a) => credit(&a.asset_code.0, &a.issuer),
        TrustLineAsset::PoolShare(pool) => format!("pool {}", hex_prefix(&pool.0.0)),
    }
}

/// Machine name of an error category, as the JSON document carries it:
/// [`error_type_name`] without spaces.
pub fn error_type_code(error_type: ScErrorType) -> &'static str {
    match error_type {
        ScErrorType::WasmVm => "wasm_vm",
        other => error_type_name(other),
    }
}

fn hex_prefix(bytes: &[u8; 32]) -> String {
    format!("{}…", hex::encode(&bytes[..8]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{diag, sym};
    use stellar_xdr::{
        ContractId, Hash, Int128Parts, Int256Parts, LedgerKeyAccount, LedgerKeyContractCode,
        LedgerKeyContractData, LedgerKeyTtl, ScAddress, ScBytes, ScMap, ScMapEntry, ScString,
        ScVec, UInt128Parts, UInt256Parts,
    };

    fn i128(n: i128) -> ScVal {
        ScVal::I128(Int128Parts {
            hi: (n >> 64) as i64,
            lo: n as u64,
        })
    }

    fn vec(items: Vec<ScVal>) -> ScVal {
        ScVal::Vec(Some(ScVec(items.try_into().unwrap())))
    }

    #[test]
    fn describes_errors_one_way() {
        assert_eq!(describe_error(&ScError::Contract(3)), "contract error #3");
        assert_eq!(
            describe_error(&ScError::Auth(ScErrorCode::InvalidAction)),
            "auth error InvalidAction"
        );
        assert_eq!(
            describe_error(&ScError::WasmVm(ScErrorCode::IndexBounds)),
            "wasm vm error IndexBounds"
        );
        // An error inside a value reads exactly like the headline.
        assert_eq!(
            scval(&ScVal::Error(ScError::Storage(ScErrorCode::ExceededLimit))),
            "storage error ExceededLimit"
        );
        assert_eq!(error_code(&ScError::Contract(9)), None);
        assert_eq!(
            error_code(&ScError::Budget(ScErrorCode::ExceededLimit)),
            Some(ScErrorCode::ExceededLimit)
        );
    }

    #[test]
    fn names_result_codes() {
        use InvokeHostFunctionResult as R;
        assert_eq!(result_code(&R::Trapped), "trapped");
        assert_eq!(
            result_code(&R::InsufficientRefundableFee),
            "insufficient_refundable_fee"
        );
        assert_eq!(result_code(&R::Success(Hash([0xab; 32]))), "success");
    }

    #[test]
    fn formats_integers_in_decimal() {
        assert_eq!(scval(&i128(-40)), "-40");
        assert_eq!(scval(&i128(i128::MIN)), i128::MIN.to_string());
        assert_eq!(scval(&i128(i128::MAX)), i128::MAX.to_string());
        assert_eq!(
            scval(&ScVal::U128(UInt128Parts { hi: 1, lo: 0 })),
            "18446744073709551616"
        );
        assert_eq!(
            scval(&ScVal::U256(UInt256Parts {
                hi_hi: 0,
                hi_lo: 0,
                lo_hi: 1,
                lo_lo: 5
            })),
            "18446744073709551621"
        );
        assert_eq!(
            scval(&ScVal::U256(UInt256Parts {
                hi_hi: u64::MAX,
                hi_lo: u64::MAX,
                lo_hi: u64::MAX,
                lo_lo: u64::MAX
            })),
            "115792089237316195423570985008687907853269984665640564039457584007913129639935"
        );
        assert_eq!(
            scval(&ScVal::I256(Int256Parts {
                hi_hi: 0,
                hi_lo: 0,
                lo_hi: 0,
                lo_lo: 0
            })),
            "0"
        );
        assert_eq!(
            scval(&ScVal::I256(Int256Parts {
                hi_hi: -1,
                hi_lo: u64::MAX,
                lo_hi: u64::MAX,
                lo_lo: u64::MAX
            })),
            "-1"
        );
        assert_eq!(
            scval(&ScVal::I256(Int256Parts {
                hi_hi: i64::MIN,
                hi_lo: 0,
                lo_hi: 0,
                lo_lo: 0
            })),
            "-57896044618658097711785492504343953926634992332820282019728792003956564819968"
        );
    }

    #[test]
    fn formats_compound_values() {
        let bytes = |n: usize| ScVal::Bytes(ScBytes(vec![0xab; n].try_into().unwrap()));
        assert_eq!(scval(&bytes(2)), "0xabab");
        assert_eq!(scval(&bytes(16)), format!("0x{}", "ab".repeat(16)));
        assert_eq!(scval(&bytes(17)), "0xabababababababab…(17 bytes)");

        let string = ScVal::String(ScString("say \"hi\"".try_into().unwrap()));
        assert_eq!(scval(&string), r#""say \"hi\"""#);

        let map = ScVal::Map(Some(ScMap(
            vec![ScMapEntry {
                key: sym("amount"),
                val: i128(100),
            }]
            .try_into()
            .unwrap(),
        )));
        assert_eq!(
            scval(&vec(vec![ScVal::Void, ScVal::Bool(true), map])),
            "[void, true, {amount: 100}]"
        );
        assert_eq!(scval(&ScVal::Vec(None)), "[]");
        assert_eq!(scval(&ScVal::Map(None)), "{}");
    }

    #[test]
    fn elides_deep_nesting() {
        let mut value = ScVal::U32(7);
        for _ in 0..6 {
            value = vec(vec![value]);
        }
        assert_eq!(scval(&value), "[[[[[…]]]]]");
    }

    #[test]
    fn renders_diagnostics() {
        let event = diag(
            Some([1; 32]),
            vec![sym("log"), ScVal::U32(1)],
            ScVal::Void,
            true,
        );
        assert_eq!(
            diagnostic(&event),
            format!("ok [{}] log, 1 void", strkey::contract(&[1; 32]))
        );
        let event = diag(None, vec![sym("error")], ScVal::Bool(false), false);
        assert_eq!(diagnostic(&event), "fail [-] error false");
    }

    #[test]
    fn labels_ledger_keys() {
        let account = LedgerKey::Account(LedgerKeyAccount {
            account_id: crate::test_support::account([2; 32]),
        });
        assert_eq!(
            ledger_key(&account),
            format!("account {}", strkey::account(&[2; 32]))
        );

        let data = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([3; 32]))),
            key: sym("Balance"),
            durability: ContractDataDurability::Temporary,
        });
        let label = ledger_key(&data);
        assert!(label.starts_with("contract data C…"), "{label}");
        assert!(label.ends_with(" Balance (temporary)"), "{label}");

        let code = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash([0xcd; 32]),
        });
        assert_eq!(ledger_key(&code), "contract code cdcdcdcdcdcdcdcd…");
        let ttl = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: Hash([0xef; 32]),
        });
        assert_eq!(ledger_key(&ttl), "ttl efefefefefefefef…");
    }

    #[test]
    fn large_collections_are_capped() {
        let numbers = |n: u32| -> ScVal {
            ScVal::Vec(Some(ScVec(
                (0..n)
                    .map(ScVal::U32)
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap(),
            )))
        };
        assert_eq!(scval(&numbers(8)), "[0, 1, 2, 3, 4, 5, 6, 7]");
        assert_eq!(scval(&numbers(9)), "[0, 1, 2, 3, 4, 5, 6, 7, … 1 more]");
        assert_eq!(
            scval(&numbers(5_000)),
            "[0, 1, 2, 3, 4, 5, 6, 7, … 4992 more]"
        );

        let map = ScVal::Map(Some(ScMap(
            (0..10u32)
                .map(|n| ScMapEntry {
                    key: ScVal::U32(n),
                    val: ScVal::Bool(true),
                })
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),
        )));
        assert!(
            scval(&map).ends_with("7: true, … 2 more}"),
            "{}",
            scval(&map)
        );
    }

    #[test]
    fn muxed_addresses_keep_their_id() {
        use stellar_xdr::{MuxedEd25519Account, Uint256};
        let muxed = |id| {
            scval(&ScVal::Address(ScAddress::MuxedAccount(
                MuxedEd25519Account {
                    id,
                    ed25519: Uint256([7; 32]),
                },
            )))
        };
        assert!(muxed(1).starts_with('M'), "{}", muxed(1));
        assert_ne!(muxed(1), muxed(2));
    }

    #[test]
    fn trustlines_name_their_asset() {
        use stellar_xdr::{
            AccountId, AlphaNum4, AssetCode4, LedgerKeyTrustLine, PublicKey, TrustLineAsset,
            Uint256,
        };
        let account = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1; 32])));
        let trustline = |asset| {
            ledger_key(&LedgerKey::Trustline(LedgerKeyTrustLine {
                account_id: account.clone(),
                asset,
            }))
        };
        let usdc = trustline(TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USDC"),
            issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2; 32]))),
        }));
        let eur = trustline(TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2; 32]))),
        }));
        assert!(usdc.contains(" USDC:G…"), "{usdc}");
        assert!(eur.contains(" EUR:G…"), "{eur}");
        assert_ne!(usdc, eur);
    }
}
