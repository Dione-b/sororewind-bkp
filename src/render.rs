//! render: turn a completed replay into something a person can read.
//!
//! Two renderers over the same report: a human narrative and a JSON document.
//! Colour is applied here and nowhere else, so the same bytes come out whether
//! or not a terminal is attached.

use std::io::IsTerminal;

use owo_colors::OwoColorize;
use serde_json::json;

use crate::replay::Fidelity;
use crate::story::Story;

/// Budget actually consumed by the local replay, next to the limits it ran
/// under.
#[derive(Debug, Clone, Copy)]
pub struct BudgetReport {
    pub cpu_insns: u64,
    pub cpu_limit: u64,
    pub mem_bytes: u64,
    pub mem_limit: u64,
}

/// How far the ledger has moved since the transaction ran.
#[derive(Debug, Clone, Default)]
pub struct DriftReport {
    /// Footprint slots whose entry was written at or after the transaction's
    /// own ledger — the value we replayed may not be the value the host saw.
    pub drifted: Vec<String>,
    /// Footprint slots with no live entry at all.
    pub missing: usize,
    pub total: usize,
    /// Whether every cost parameter came from a real `ConfigSetting` entry.
    pub config_from_ledger: bool,
}

/// Everything both renderers need.
#[derive(Debug, Clone)]
pub struct Report {
    pub hash: String,
    pub network: String,
    pub rpc_url: String,
    pub ledger: u32,
    pub protocol: u32,
    pub close_time: u64,
    pub status: String,

    pub fidelity: Fidelity,
    pub onchain_label: String,
    pub local_label: String,

    pub story: Story,
    /// Contract-declared name for the failing error code, when we could read
    /// the contract's spec.
    pub error_name: Option<String>,
    /// Where that name came from, shown under `-v`.
    pub error_origin: Option<String>,

    pub budget: BudgetReport,
    pub drift: DriftReport,
    /// Raw diagnostic summaries, `-vv` only.
    pub onchain_diagnostics: Vec<String>,
    pub local_diagnostics: Vec<String>,
}

/// Should ANSI colour be emitted?
///
/// `--no-color` wins, then `NO_COLOR`, then "is this a terminal".
pub fn use_color(no_color: bool) -> bool {
    if no_color || std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

struct Palette {
    on: bool,
}

impl Palette {
    fn new(on: bool) -> Self {
        Self { on }
    }

    fn paint(&self, text: &str, color: fn(&str) -> String) -> String {
        if self.on {
            color(text)
        } else {
            text.to_string()
        }
    }

    fn red(&self, t: &str) -> String {
        self.paint(t, |s| s.red().to_string())
    }
    fn green(&self, t: &str) -> String {
        self.paint(t, |s| s.green().to_string())
    }
    fn yellow(&self, t: &str) -> String {
        self.paint(t, |s| s.yellow().to_string())
    }
    fn cyan(&self, t: &str) -> String {
        self.paint(t, |s| s.cyan().to_string())
    }
    fn bold(&self, t: &str) -> String {
        self.paint(t, |s| s.bold().to_string())
    }
    fn dim(&self, t: &str) -> String {
        self.paint(t, |s| s.dimmed().to_string())
    }
}

pub fn human(report: &Report, color: bool, verbose: u8) -> String {
    let p = Palette::new(color);
    let mut out = String::new();

    out.push_str(&format!(
        "{}  {}  {}\n",
        p.bold("sororewind"),
        p.cyan(&short_hash(&report.hash)),
        p.bold(&report.network),
    ));
    out.push_str(&format!(
        "{}\n",
        p.dim(&format!(
            "ledger {} · protocol {} · {}",
            report.ledger,
            report.protocol,
            utc(report.close_time)
        ))
    ));
    out.push('\n');

    // Fidelity is the whole point of the tool: say it first, say it plainly.
    match &report.fidelity {
        Fidelity::Match => {
            out.push_str(&format!("{}\n", p.green("✓ replay matches network")));
        }
        Fidelity::Diverged { onchain, local } => {
            out.push_str(&format!(
                "{}\n",
                p.yellow(&format!(
                    "⚠ replay diverged from the network ({onchain} vs {local})"
                ))
            ));
            out.push_str(&format!(
                "{}\n",
                p.dim("  the ledger has almost certainly moved since this transaction ran")
            ));
        }
    }
    out.push_str(&format!(
        "{}\n",
        p.dim(&format!(
            "  on-chain  {}\n  local     {}",
            report.onchain_label, report.local_label
        ))
    ));
    out.push('\n');

    if let Some(failure) = &report.story.failure {
        out.push_str(&format!("{}\n", p.bold("where it broke")));
        out.push_str(&format!("  {}\n", describe_error(report, &p)));
        if !failure.message.is_empty() {
            out.push_str(&format!("  {}\n", p.dim(&format!("{:?}", failure.message))));
        }
        match (&failure.contract, &failure.function) {
            (Some(contract), function) => {
                let at = match function {
                    Some(f) => format!("{contract}::{f}"),
                    None => contract.clone(),
                };
                out.push_str(&format!(
                    "  {}\n",
                    p.dim(&format!("in {at} (depth {})", failure.depth))
                ));
            }
            _ => {}
        }
        if verbose > 0 {
            if let Some(origin) = &report.error_origin {
                out.push_str(&format!("  {}\n", p.dim(&format!("name from {origin}"))));
            }
        }
        if verbose > 1 && failure.trail.len() > 1 {
            out.push_str(&format!(
                "  {}\n",
                p.dim(&format!(
                    "{} error diagnostics: {}",
                    failure.trail.len(),
                    failure.trail.join(" → ")
                ))
            ));
        }
        out.push('\n');
    }

    out.push_str(&format!("{}\n", p.bold("call tree")));
    if report.story.calls.is_empty() {
        out.push_str(&format!(
            "  {}\n",
            p.dim("no contract was entered before the failure")
        ));
    }
    for call in &report.story.calls {
        let indent = if call.depth == 0 {
            String::new()
        } else {
            format!("{}└─ ", "   ".repeat(call.depth - 1))
        };
        let head = match &call.contract {
            Some(contract) => format!("{contract}::{}", call.function),
            None => call.function.clone(),
        };
        let mut line = format!("{indent}{head}({})", call.args.join(", "));

        if let Some(returned) = &call.returned {
            if verbose > 0 {
                line.push_str(&format!(" → {returned}"));
            }
        }
        if call.recovered && verbose > 0 {
            line.push_str(&format!(" {}", p.dim("(recovered by try_call)")));
        }
        if call.is_failure {
            out.push_str(&format!("{}\n", p.red(&format!("{line}  ✗"))));
        } else if call.open {
            out.push_str(&format!("{}\n", p.yellow(&line)));
        } else {
            out.push_str(&format!("{line}\n"));
        }

        for log in &call.logs {
            let log_indent = format!("{}  · ", if call.depth == 0 { "  " } else { "     " });
            out.push_str(&format!("{}{}\n", p.dim(&log_indent), p.dim(log)));
        }
    }
    out.push('\n');

    if verbose > 0 {
        let b = &report.budget;
        out.push_str(&format!(
            "{}\n",
            p.dim(&format!(
                "budget {} / {} cpu instructions · {} / {} memory",
                grouped(b.cpu_insns),
                grouped(b.cpu_limit),
                human_bytes(b.mem_bytes),
                human_bytes(b.mem_limit),
            ))
        ));
        out.push_str(&format!(
            "{}\n",
            p.dim(&format!(
                "footprint {} entries · {} drifted · {} missing · cost params {}",
                report.drift.total,
                report.drift.drifted.len(),
                report.drift.missing,
                if report.drift.config_from_ledger {
                    "from the ledger"
                } else {
                    "protocol defaults (the RPC would not serve them)"
                }
            ))
        ));
        for key in &report.drift.drifted {
            out.push_str(&format!("  {} {key}\n", p.yellow("drifted:")));
        }
        out.push('\n');
    }

    if verbose > 1 {
        for (title, diagnostics) in [
            ("on-chain diagnostics", &report.onchain_diagnostics),
            ("local diagnostics", &report.local_diagnostics),
        ] {
            if diagnostics.is_empty() {
                continue;
            }
            out.push_str(&format!("{}\n", p.bold(title)));
            for line in diagnostics {
                out.push_str(&format!("  {line}\n"));
            }
            out.push('\n');
        }
    }

    out
}

fn describe_error(report: &Report, p: &Palette) -> String {
    let Some(failure) = &report.story.failure else {
        return p.red("the replay failed");
    };
    match failure.contract_error_code() {
        Some(code) => match &report.error_name {
            Some(name) => p.red(&format!("contract error #{code} · {name}")),
            None => p.red(&format!("contract error #{code}")),
        },
        None => {
            let type_name = failure
                .error_type()
                .map(|t| format!("{t:?}").to_lowercase())
                .unwrap_or_else(|| "host".to_string());
            let code = failure
                .sc_error
                .as_ref()
                .and_then(sc_error_code)
                .map(|c| format!(" {c:?}"))
                .unwrap_or_default();
            p.red(&format!("{type_name} error{code}"))
        }
    }
}

fn sc_error_code(e: &stellar_xdr::ScError) -> Option<stellar_xdr::ScErrorCode> {
    use stellar_xdr::ScError::*;
    match e {
        Contract(_) => None,
        WasmVm(c) | Context(c) | Storage(c) | Object(c) | Crypto(c) | Events(c) | Budget(c)
        | Value(c) | Auth(c) => Some(*c),
    }
}

pub fn json(report: &Report) -> String {
    let failure = report.story.failure.as_ref().map(|f| {
        json!({
            "type": f.error_type().map(|t| format!("{t:?}")),
            "code": f.contract_error_code(),
            "name": report.error_name,
            "nameOrigin": report.error_origin,
            "message": f.message,
            "contract": f.contract,
            "function": f.function,
            "depth": f.depth,
            "trail": f.trail,
        })
    });

    let calls: Vec<_> = report
        .story
        .calls
        .iter()
        .map(|c| {
            json!({
                "depth": c.depth,
                "contract": c.contract,
                "function": c.function,
                "args": c.args,
                "returned": c.returned,
                "open": c.open,
                "recovered": c.recovered,
                "failure": c.is_failure,
                "logs": c.logs,
            })
        })
        .collect();

    let fidelity = match &report.fidelity {
        Fidelity::Match => json!({ "match": true }),
        Fidelity::Diverged { onchain, local } => {
            json!({ "match": false, "onchain": onchain, "local": local })
        }
    };

    let document = json!({
        "hash": report.hash,
        "network": report.network,
        "rpc": report.rpc_url,
        "ledger": report.ledger,
        "protocol": report.protocol,
        "closeTime": report.close_time,
        "status": report.status,
        "fidelity": fidelity,
        "result": {
            "onchain": report.onchain_label,
            "local": report.local_label,
        },
        "failure": failure,
        "calls": calls,
        "budget": {
            "cpuInsns": report.budget.cpu_insns,
            "cpuLimit": report.budget.cpu_limit,
            "memBytes": report.budget.mem_bytes,
            "memLimit": report.budget.mem_limit,
        },
        "footprint": {
            "entries": report.drift.total,
            "drifted": report.drift.drifted,
            "missing": report.drift.missing,
            "configFromLedger": report.drift.config_from_ledger,
        },
        "diagnostics": {
            "onchain": report.onchain_diagnostics,
            "local": report.local_diagnostics,
        },
    });

    // Pretty-printed: this output is meant to be read and diffed.
    serde_json::to_string_pretty(&document).expect("report serializes cleanly")
}

fn short_hash(hash: &str) -> String {
    if hash.len() > 12 {
        format!("{}…{}", &hash[..6], &hash[hash.len() - 4..])
    } else {
        hash.to_string()
    }
}

/// Seconds since the Unix epoch into a fixed, sortable UTC stamp.
fn utc(timestamp: u64) -> String {
    // Civil-from-days, Howard Hinnant's algorithm — no date crate for one line.
    let days = (timestamp / 86_400) as i64;
    let secs = timestamp % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_timestamps() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(1_700_000_000), "2023-11-14 22:13:20 UTC");
    }

    #[test]
    fn groups_digits() {
        assert_eq!(grouped(4_120), "4,120");
        assert_eq!(grouped(100_000_000), "100,000,000");
    }
}
