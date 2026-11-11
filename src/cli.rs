//! Command-line surface.

use std::path::PathBuf;
use std::sync::LazyLock;

use clap::{CommandFactory, FromArgMatches, Parser};

use crate::error::{Result, RewindError};
use crate::hash::TxHash;
use crate::network::Network;
use crate::pipeline::Request;
use crate::rpc::RpcClient;
use crate::transport::{FixtureManifest, FixtureTransport, HttpTransport, Recording};

/// Replay a failed Soroban transaction locally and narrate the call tree.
#[derive(Debug, Parser)]
#[command(
    name = "sororewind",
    about = "Explain a failed Soroban transaction by replaying it locally",
    long_about = "sororewind fetches a failed Soroban transaction from an RPC, rebuilds the ledger \
state it ran against, re-executes it with the same Soroban host, and prints a narrated call tree \
up to the point of failure.",
    after_help = "EXAMPLES:\n  \
        sororewind 3f9a...c21\n  \
        sororewind 3f9a...c21 --network mainnet\n  \
        sororewind 3f9a...c21 --json\n  \
        sororewind 3f9a...c21 --wasm-debug ./vault.wasm -v\n\n\
        EXIT CODES:\n  \
        0  the replay matched the network's recorded result\n  \
        1  sororewind could not replay the transaction (bad input, RPC error, ...)\n  \
        2  the replay ran but diverged from the network"
)]
pub struct Cli {
    /// Hash of the failed transaction (hex, with or without 0x). Optional with
    /// --fixture, which records the hash it was made for
    #[arg(required_unless_present = "fixture")]
    pub tx_hash: Option<TxHash>,

    /// Network the transaction lives on; selects the default RPC endpoint and
    /// is checked against the RPC's passphrase
    /// [default RPCs: testnet https://soroban-testnet.stellar.org,
    /// mainnet https://mainnet.sorobanrpc.com (third-party),
    /// futurenet https://rpc-futurenet.stellar.org, local http://localhost:8000/rpc]
    #[arg(long, short = 'n', value_enum)]
    pub network: Option<Network>,

    /// Use this RPC endpoint instead of the default for --network
    #[arg(long, value_name = "URL", conflicts_with = "fixture")]
    pub rpc_url: Option<String>,

    /// Emit machine-readable JSON instead of the human narrative
    #[arg(long)]
    pub json: bool,

    /// Read the contract's error names from this WASM file instead of the
    /// ledger (debugging aid)
    #[arg(long, value_name = "PATH")]
    pub wasm_debug: Option<PathBuf>,

    /// Disable ANSI colour output (also off when stdout is not a terminal or
    /// NO_COLOR is set)
    #[arg(long)]
    pub no_color: bool,

    /// Repeat for more detail: -v adds budget and footprint detail, -vv adds
    /// raw diagnostics and RPC tracing on stderr
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Replay from a recorded RPC fixture directory instead of the network
    #[arg(long, value_name = "DIR")]
    pub fixture: Option<PathBuf>,

    /// Write every RPC response into this directory while running
    #[arg(long, value_name = "DIR", hide = true, conflicts_with = "fixture")]
    pub record: Option<PathBuf>,
}

/// `0.1.0 (protocols 29 (soroban-env-host 29.0.2), 30 (...))`: which
/// transactions this build can replay is part of its version.
static VERSION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{} (protocols {})",
        env!("CARGO_PKG_VERSION"),
        crate::host::describe()
    )
});

impl Cli {
    pub fn command() -> clap::Command {
        <Cli as CommandFactory>::command().version(VERSION.as_str())
    }

    /// Parse arguments; clap's own error carries help, version and usage
    /// problems alike.
    pub fn try_parse_args<I, T>(args: I) -> std::result::Result<Cli, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let matches = Cli::command().try_get_matches_from(args)?;
        Cli::from_arg_matches(&matches)
    }

    /// Open the source of RPC responses the arguments ask for, and work out
    /// which transaction to replay.
    pub async fn connect(&self) -> Result<(RpcClient, Request)> {
        let (rpc, hash) = match &self.fixture {
            Some(dir) => {
                let hash = match &self.tx_hash {
                    Some(hash) => hash.clone(),
                    None => FixtureManifest::load(dir).await?.tx_hash()?,
                };
                (RpcClient::new(FixtureTransport::open(dir).await?), hash)
            }
            None => {
                // clap requires a hash whenever there is no fixture; a `Cli`
                // built by hand without one is the caller's invalid input.
                let hash = self.tx_hash.clone().ok_or(RewindError::InvalidHash {
                    hash: String::new(),
                })?;
                let url = self.rpc_url.clone().unwrap_or_else(|| {
                    self.network
                        .unwrap_or(Network::Testnet)
                        .default_rpc_url()
                        .to_string()
                });
                let http = HttpTransport::new(url)?;
                let rpc = match &self.record {
                    Some(dir) => RpcClient::new(Recording::create(http, dir, &hash).await?),
                    None => RpcClient::new(http),
                };
                (rpc, hash)
            }
        };
        Ok((
            rpc,
            Request {
                hash,
                network: self.network,
                wasm_debug: self.wasm_debug.clone(),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    const HASH: &str = "015b092ded2eec4838dee20a5d172c428faa863a74e3706cd3aeb6581df0fd16";

    fn parse(args: &[&str]) -> std::result::Result<Cli, clap::Error> {
        Cli::try_parse_args(std::iter::once("sororewind").chain(args.iter().copied()))
    }

    #[test]
    fn command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn hash_is_validated_at_parse_time() {
        let cli = parse(&[&format!("0x{}", HASH.to_uppercase())]).unwrap();
        assert_eq!(cli.tx_hash.unwrap().as_str(), HASH);
        assert_eq!(cli.network, None);

        let err = parse(&["not-a-hash"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
        assert!(
            err.to_string().contains("64 hexadecimal characters"),
            "{err}"
        );
    }

    #[test]
    fn hash_is_required_without_a_fixture() {
        assert_eq!(
            parse(&[]).unwrap_err().kind(),
            ErrorKind::MissingRequiredArgument
        );
        let cli = parse(&["--fixture", "fixtures/x"]).unwrap();
        assert!(cli.tx_hash.is_none());
    }

    #[test]
    fn fixture_conflicts_with_live_sources() {
        for flag in ["--rpc-url", "--record"] {
            let err = parse(&[HASH, "--fixture", "dir", flag, "x"]).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::ArgumentConflict, "{flag}");
        }
        // Recording a live run against a custom endpoint is fine.
        assert!(parse(&[HASH, "--rpc-url", "http://x", "--record", "dir"]).is_ok());
    }

    #[test]
    fn networks_and_verbosity() {
        for (name, network) in [
            ("testnet", Network::Testnet),
            ("mainnet", Network::Mainnet),
            ("futurenet", Network::Futurenet),
            ("local", Network::Local),
        ] {
            assert_eq!(parse(&[HASH, "-n", name]).unwrap().network, Some(network));
        }
        assert!(parse(&[HASH, "--network", "moon"]).is_err());
        assert_eq!(parse(&[HASH, "-vv"]).unwrap().verbose, 2);
    }

    #[test]
    fn version_names_the_supported_protocol() {
        let err = parse(&["--version"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        let text = err.to_string();
        let expected = format!(
            "sororewind {} (protocols {})\n",
            env!("CARGO_PKG_VERSION"),
            crate::host::describe()
        );
        assert_eq!(text, expected);
        assert!(text.contains("29 (soroban-env-host 29."), "{text}");
        assert!(text.contains("30 (soroban-env-host 30."), "{text}");
    }
}
