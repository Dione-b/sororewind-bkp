//! Command-line surface.

use std::path::PathBuf;
use std::sync::LazyLock;

use clap::{CommandFactory, FromArgMatches, Parser};

use crate::error::{Result, RewindError};
use crate::hash::TxHash;
use crate::network::Network;
use crate::pipeline::Request;
use crate::render::ColorChoice;
use crate::rpc::RpcClient;
use crate::transport::{FixtureTransport, HttpTransport, Recording};

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
    /// is checked against the RPC's passphrase. (The help text is completed
    /// with the default endpoints in `Cli::command`.)
    #[arg(long, short = 'n', value_enum)]
    pub network: Option<Network>,

    /// Use this RPC endpoint instead of the default for --network. Prefer the
    /// SOROREWIND_RPC_URL environment variable for a URL that holds an API
    /// key: arguments end up in shell history and process listings
    #[arg(long, value_name = "URL", conflicts_with = "fixture", value_parser = parse_rpc_url)]
    pub rpc_url: Option<String>,

    /// Emit machine-readable JSON instead of the human narrative
    #[arg(long)]
    pub json: bool,

    /// Read the contract's error names from this WASM file instead of the
    /// ledger (debugging aid)
    #[arg(long, value_name = "PATH", value_parser = parse_wasm_path)]
    pub wasm_debug: Option<PathBuf>,

    /// When to colour the narrative: auto colours a terminal, and honours
    /// NO_COLOR, CLICOLOR_FORCE and TERM=dumb
    #[arg(long, value_enum, value_name = "WHEN", default_value = "auto")]
    pub color: ColorChoice,

    /// Same as --color never
    #[arg(long, conflicts_with = "color")]
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

/// The environment variable that names the RPC endpoint when `--rpc-url` is
/// not given.
pub const RPC_URL_ENV: &str = "SOROREWIND_RPC_URL";

/// The endpoint an environment value names; an empty value names none.
fn rpc_url_from(value: Option<String>) -> Option<String> {
    value.filter(|url| !url.trim().is_empty())
}

/// An RPC endpoint has to be an absolute http(s) URL: `localhost:8000` parses
/// as a URL with the scheme `localhost`, and would only fail once sent.
fn parse_rpc_url(text: &str) -> std::result::Result<String, String> {
    let text = text.trim();
    let url =
        reqwest::Url::parse(text).map_err(|e| format!("{e} (expected http://… or https://…)"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "the scheme is `{}`, expected http or https (did you mean http://{text}?)",
            url.scheme()
        ));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("it has no host".to_string());
    }
    Ok(text.to_string())
}

/// `--wasm-debug` is only read when the failure turns out to be a contract
/// error; a path that does not exist should not wait until then to be told.
fn parse_wasm_path(text: &str) -> std::result::Result<PathBuf, String> {
    let path = PathBuf::from(text);
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => Ok(path),
        Ok(_) => Err("it is not a file".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// `testnet https://…, mainnet https://… (third-party), …`, from the
/// endpoints the code actually uses.
fn default_rpcs() -> String {
    Network::ALL
        .into_iter()
        .map(|network| {
            format!(
                "{network} {}{}",
                network.default_rpc_url(),
                if network == Network::Mainnet {
                    " (third-party)"
                } else {
                    ""
                }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

impl Cli {
    pub fn command() -> clap::Command {
        <Cli as CommandFactory>::command()
            .version(VERSION.as_str())
            .mut_arg("network", |arg| {
                arg.help(format!(
                    "Network the transaction lives on; selects the default RPC endpoint and is \
                     checked against the RPC's passphrase [default RPCs: {}]",
                    default_rpcs()
                ))
            })
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

    /// The colour choice the flags amount to.
    pub fn color_choice(&self) -> ColorChoice {
        if self.no_color {
            ColorChoice::Never
        } else {
            self.color
        }
    }

    /// Open the source of RPC responses the arguments ask for, and work out
    /// which transaction to replay.
    pub async fn connect(&self) -> Result<(RpcClient, Request)> {
        let (rpc, hash) = match &self.fixture {
            Some(dir) => {
                let fixture = FixtureTransport::open(dir).await?;
                // A fixture holds one transaction; a different hash would
                // only fail later, on a response file it does not have.
                let hash = fixture.hash_for(self.tx_hash.as_ref())?;
                (RpcClient::new(fixture), hash)
            }
            None => {
                // clap requires a hash whenever there is no fixture; a `Cli`
                // built by hand without one is the caller's invalid input.
                let hash = self.tx_hash.clone().ok_or(RewindError::MissingHash)?;
                let url = self
                    .rpc_url
                    .clone()
                    .map(Ok)
                    .or_else(|| {
                        // The flag was validated by clap; the variable was not.
                        rpc_url_from(std::env::var(RPC_URL_ENV).ok()).map(|url| {
                            parse_rpc_url(&url).map_err(|reason| RewindError::InvalidRpcUrl {
                                reason: format!("{RPC_URL_ENV}: {reason}"),
                            })
                        })
                    })
                    .transpose()?
                    .unwrap_or_else(|| {
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
            let err = parse(&[HASH, "--fixture", "dir", flag, "http://x"]).unwrap_err();
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
    fn rpc_urls_must_be_http() {
        for url in [
            "http://localhost:8000/rpc",
            "https://rpc.example/v1/key?x=1",
        ] {
            assert_eq!(
                parse(&[HASH, "--rpc-url", url]).unwrap().rpc_url.as_deref(),
                Some(url)
            );
        }
        let message = |url: &str| parse(&[HASH, "--rpc-url", url]).unwrap_err().to_string();
        // Parses as a URL whose scheme is `localhost`.
        assert!(
            message("localhost:8000").contains("did you mean http://localhost:8000?"),
            "{}",
            message("localhost:8000")
        );
        assert!(message("ftp://rpc.example").contains("expected http or https"));
        assert!(message("rpc.example").contains("expected http://"));
    }

    #[test]
    fn a_wasm_debug_path_must_be_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("contract.wasm");
        std::fs::write(&file, b"\0asm").unwrap();
        let file = file.display().to_string();
        assert!(parse(&[HASH, "--wasm-debug", &file]).is_ok());

        let missing = dir.path().join("nope.wasm").display().to_string();
        let err = parse(&[HASH, "--wasm-debug", &missing]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
        let directory = dir.path().display().to_string();
        let err = parse(&[HASH, "--wasm-debug", &directory]).unwrap_err();
        assert!(err.to_string().contains("it is not a file"), "{err}");
    }

    #[test]
    fn help_lists_the_default_rpcs_the_code_uses() {
        let help = Cli::command().render_long_help().to_string();
        for network in Network::ALL {
            assert!(
                help.contains(network.default_rpc_url()),
                "{network}: {help}"
            );
        }
    }

    #[tokio::test]
    async fn a_missing_hash_is_named_as_such() {
        let cli = Cli {
            tx_hash: None,
            ..parse(&[HASH]).unwrap()
        };
        let err = cli.connect().await.err().unwrap();
        assert!(matches!(err, RewindError::MissingHash), "{err}");
        assert!(!err.to_string().contains("``"));
    }

    #[test]
    fn the_environment_can_name_the_rpc() {
        assert_eq!(
            rpc_url_from(Some("https://rpc.example/key".to_string())).as_deref(),
            Some("https://rpc.example/key")
        );
        assert_eq!(rpc_url_from(Some("  ".to_string())), None);
        assert_eq!(rpc_url_from(None), None);
    }

    #[test]
    fn colour_flags() {
        assert_eq!(parse(&[HASH]).unwrap().color_choice(), ColorChoice::Auto);
        for (flags, choice) in [
            (&["--color", "always"][..], ColorChoice::Always),
            (&["--color=never"][..], ColorChoice::Never),
            (&["--color", "auto"][..], ColorChoice::Auto),
            (&["--no-color"][..], ColorChoice::Never),
        ] {
            let args = [&[HASH][..], flags].concat();
            assert_eq!(parse(&args).unwrap().color_choice(), choice, "{flags:?}");
        }
        assert!(parse(&[HASH, "--color", "sometimes"]).is_err());
        let err = parse(&[HASH, "--no-color", "--color", "always"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
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
