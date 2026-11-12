//! Which network a transaction lives on.
//!
//! `--network` only picks a default RPC endpoint. The network's identity is
//! its passphrase, which every RPC reports through `getNetwork`; that is what
//! the report prints and what the host hashes into the network id.

use std::fmt;

use clap::ValueEnum;

/// Networks sororewind knows a default RPC endpoint and passphrase for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Network {
    /// The SDF test network.
    Testnet,
    /// The public network (pubnet).
    Mainnet,
    /// The SDF network that previews upcoming protocol changes.
    Futurenet,
    /// A local quickstart / standalone network.
    Local,
}

impl Network {
    /// Every known network, in declaration order.
    pub const ALL: [Network; 4] = [
        Network::Testnet,
        Network::Mainnet,
        Network::Futurenet,
        Network::Local,
    ];

    /// The name `--network` accepts and the report prints.
    pub fn name(self) -> &'static str {
        match self {
            Network::Testnet => "testnet",
            Network::Mainnet => "mainnet",
            Network::Futurenet => "futurenet",
            Network::Local => "local",
        }
    }

    /// The passphrase that identifies the network, as `getNetwork` reports it.
    pub fn passphrase(self) -> &'static str {
        match self {
            Network::Testnet => "Test SDF Network ; September 2015",
            Network::Mainnet => "Public Global Stellar Network ; September 2015",
            Network::Futurenet => "Test SDF Future Network ; October 2022",
            Network::Local => "Standalone Network ; February 2017",
        }
    }

    /// The endpoint used when `--rpc-url` is not given.
    ///
    /// The Stellar Development Foundation runs public RPCs for testnet and
    /// futurenet only. The mainnet default is a third-party public endpoint;
    /// pass `--rpc-url` to use your own provider.
    pub fn default_rpc_url(self) -> &'static str {
        match self {
            Network::Testnet => "https://soroban-testnet.stellar.org:443",
            Network::Mainnet => "https://mainnet.sorobanrpc.com",
            Network::Futurenet => "https://rpc-futurenet.stellar.org:443",
            Network::Local => "http://localhost:8000/rpc",
        }
    }

    /// The known network with exactly this passphrase, if any.
    pub fn from_passphrase(passphrase: &str) -> Option<Network> {
        Network::ALL
            .into_iter()
            .find(|n| n.passphrase() == passphrase)
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The network an RPC actually serves, derived from its passphrase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkId {
    /// A network sororewind has a name for.
    Known(Network),
    /// A passphrase sororewind has no name for; printed verbatim.
    Custom(String),
}

impl NetworkId {
    /// Name the network behind `passphrase`, falling back to
    /// [`NetworkId::Custom`].
    pub fn from_passphrase(passphrase: &str) -> NetworkId {
        match Network::from_passphrase(passphrase) {
            Some(network) => NetworkId::Known(network),
            None => NetworkId::Custom(passphrase.to_string()),
        }
    }
}

impl fmt::Display for NetworkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkId::Known(network) => network.fmt(f),
            NetworkId::Custom(passphrase) => write!(f, "custom network \"{passphrase}\""),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passphrases_round_trip() {
        for network in Network::ALL {
            assert_eq!(
                Network::from_passphrase(network.passphrase()),
                Some(network)
            );
            assert_eq!(
                NetworkId::from_passphrase(network.passphrase()).to_string(),
                network.name()
            );
        }
    }

    #[test]
    fn unknown_passphrase_is_printed_verbatim() {
        let id = NetworkId::from_passphrase("My Private Net ; 2026");
        assert_eq!(id.to_string(), "custom network \"My Private Net ; 2026\"");
    }
}
