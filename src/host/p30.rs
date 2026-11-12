//! The protocol-30 host: `proto.rs` compiled against soroban-env-host 30.

use soroban_env_host_p30 as soroban_env_host;

// Mounted once per host on purpose: the same source, compiled against a
// different `soroban_env_host`.
#[allow(clippy::duplicate_mod)]
#[path = "proto.rs"]
mod proto;

pub use proto::{PROTOCOL, VERSION, fees, invoke};
