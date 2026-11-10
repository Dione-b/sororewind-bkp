//! sororewind — replay a failed Soroban transaction and narrate the failure.
//!
//! The binary is a thin shell: parse arguments, run the pipeline, map the
//! outcome onto an exit code. Everything else lives in the pipeline so the
//! offline fixtures exercise exactly the same path as a live run.

mod cli;
mod error;
mod pipeline;
mod render;
mod replay;
mod rpc;
mod spec;
mod state;
mod story;
mod strkey;
mod tx;

use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = cli::Cli::parse();

    match pipeline::run(&cli).await {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("sororewind: {err}");
            std::process::exit(pipeline::EXIT_ERROR);
        }
    }
}
