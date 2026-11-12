//! sororewind — replay a failed Soroban transaction and narrate the failure.
//!
//! The library is the whole tool; the binary only parses arguments, prints the
//! report and maps the outcome onto an exit code. The pipeline, one module per
//! stage:
//!
//! ```text
//! fetch (rpc, transport) → decode (tx) → reconstruct (state, history) → replay (replay)
//!       → narrate (story, spec) → report (report, render)
//! ```
//!
//! [`pipeline::run`] wires the stages together. It takes an [`rpc::RpcClient`]
//! over any [`transport::Transport`], so the offline fixtures and the tests
//! exercise exactly the same path as a live run.

// Tests assert with `unwrap`/`expect`; production code may not.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod cli;
pub mod codec;
pub mod discover;
pub mod display;
pub mod error;
pub mod hash;
pub mod history;
pub mod host;
pub mod network;
pub mod pipeline;
pub mod render;
pub mod replay;
pub mod report;
pub mod rpc;
pub mod spec;
pub mod state;
pub mod story;
pub mod strkey;
pub mod transport;
pub mod tx;

#[cfg(test)]
mod test_support;

pub use error::{Result, RewindError};
pub use hash::TxHash;
pub use network::Network;
pub use pipeline::{Request, run};
pub use report::{Outcome, Report};

/// The replay ran and reproduced the network's recorded result.
pub const EXIT_OK: u8 = 0;
/// The tool itself failed: bad input, unreachable RPC, un-replayable state.
pub const EXIT_ERROR: u8 = 1;
/// The replay ran but disagreed with the network.
pub const EXIT_DIVERGED: u8 = 2;

/// The process exit code for a finished run.
pub fn exit_code(result: &Result<Report>) -> u8 {
    match result.as_ref().map(Report::outcome) {
        Ok(Outcome::Matched) => EXIT_OK,
        Ok(Outcome::Diverged) => EXIT_DIVERGED,
        Err(_) => EXIT_ERROR,
    }
}
