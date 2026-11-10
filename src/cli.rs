use clap::{Parser, ValueEnum};

/// Replay a failed Soroban transaction locally and narrate the call tree.
#[derive(Debug, Parser)]
#[command(
    name = "sororewind",
    version,
    about = "Explain a failed Soroban transaction by replaying it locally",
    long_about = "sororewind fetches a failed Soroban transaction from an RPC, rebuilds the ledger \
state it ran against, re-executes it with the same Soroban host, and prints a narrated call tree \
up to the point of failure.",
    after_help = "EXAMPLES:\n  \
        sororewind 3f9a...c21\n  \
        sororewind 3f9a...c21 --network mainnet\n  \
        sororewind 3f9a...c21 --json\n  \
        sororewind 3f9a...c21 --wasm-debug ./vault.wasm -v"
)]
pub struct Cli {
    /// Hash of the failed transaction (hex, with or without 0x)
    pub tx_hash: String,

    /// Network the transaction lives on
    #[arg(long, short = 'n', value_enum, default_value_t = Network::Testnet)]
    pub network: Network,

    /// Override the RPC endpoint (defaults to the public RPC for --network)
    #[arg(long)]
    pub rpc_url: Option<String>,

    /// Emit machine-readable JSON instead of the human narrative
    #[arg(long)]
    pub json: bool,

    /// Load contract code from this WASM file instead of the ledger (debugging aid)
    #[arg(long)]
    pub wasm_debug: Option<std::path::PathBuf>,

    /// Disable ANSI colour output (also honoured when stdout is not a TTY)
    #[arg(long)]
    pub no_color: bool,

    /// Repeat for more detail: -v adds budget/drift detail, -vv adds raw diagnostics
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Replay from a recorded RPC fixture directory instead of the network
    #[arg(long, value_name = "DIR")]
    pub fixture: Option<std::path::PathBuf>,

    /// Write every RPC response into this directory while running
    #[arg(long, value_name = "DIR", hide = true)]
    pub record: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Network {
    Testnet,
    Mainnet,
}

impl Network {
    pub fn name(self) -> &'static str {
        match self {
            Network::Testnet => "testnet",
            Network::Mainnet => "mainnet",
        }
    }

    pub fn default_rpc_url(self) -> &'static str {
        match self {
            Network::Testnet => "https://soroban-testnet.stellar.org:443",
            Network::Mainnet => "https://mainnet.sorobanrpc.com",
        }
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
