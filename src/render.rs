//! render: turn a completed replay into something a person can read.
//!
//! Two renderers over the same [`Report`]: a human narrative and a JSON
//! document. Colour is applied here and nowhere else — and only to the
//! narrative on stdout; anything on stderr is plain.

use std::fmt::{self, Write};
use std::io::IsTerminal;

use owo_colors::OwoColorize;

use crate::replay::{Divergence, PrngSeed};
use crate::report::Report;
use crate::story::{Call, Failure};

/// How much of the report the narrative shows. Derived from `-v`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Detail {
    /// Fidelity, the failure and the call tree.
    Summary,
    /// Adds return values, budget and footprint detail (`-v`).
    Budget,
    /// Adds the raw diagnostic events (`-vv`).
    Diagnostics,
}

impl Detail {
    pub fn from_verbosity(verbose: u8) -> Detail {
        match verbose {
            0 => Detail::Summary,
            1 => Detail::Budget,
            _ => Detail::Diagnostics,
        }
    }
}

/// Should ANSI colour be emitted on stdout?
///
/// `--no-color` wins, then `NO_COLOR`, then "is stdout a terminal".
pub fn use_color(no_color: bool) -> bool {
    if no_color || std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// Colour that can be switched off.
///
/// Decision (hand-rolled, kept): `owo-colors` has its own conditional colour
/// (`if_supports_color`, `set_override`), but it works through process-global
/// state and an extra dependency, whereas the renderer needs an explicit flag
/// so tests can render both ways side by side. Six one-line methods do that.
struct Palette {
    on: bool,
}

impl Palette {
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

/// The JSON document: the report itself, pretty-printed because this output
/// is meant to be read and diffed as well as parsed.
pub fn json(report: &Report) -> serde_json::Result<String> {
    serde_json::to_string_pretty(report)
}

/// The human narrative.
pub fn human(report: &Report, color: bool, detail: Detail) -> String {
    let mut out = String::new();
    // Writing into a `String` cannot fail.
    let _ = write_human(&mut out, report, &Palette { on: color }, detail);
    out
}

fn write_human(out: &mut impl Write, report: &Report, p: &Palette, detail: Detail) -> fmt::Result {
    header(out, report, p)?;
    fidelity(out, report, p)?;
    if let Some(failure) = &report.story.failure {
        where_it_broke(out, failure, p, detail)?;
    }
    call_tree(out, &report.story.calls, p, detail)?;
    if detail >= Detail::Budget {
        budget(out, report, p)?;
    }
    if detail >= Detail::Budget || !report.fidelity.matches {
        drift(out, report, p, detail)?;
    }
    if detail >= Detail::Diagnostics {
        diagnostics(out, report, p)?;
    }
    Ok(())
}

fn header(out: &mut impl Write, report: &Report, p: &Palette) -> fmt::Result {
    writeln!(
        out,
        "{}  {}  {}",
        p.bold("sororewind"),
        p.cyan(&report.hash.short()),
        p.bold(&report.network),
    )?;
    let fee_bump = if report.fee_bump { " · fee-bump" } else { "" };
    writeln!(
        out,
        "{}",
        p.dim(&format!(
            "ledger {} · protocol {} · {}{fee_bump}",
            report.ledger,
            report.protocol,
            utc(report.close_time)
        ))
    )?;
    writeln!(out)
}

/// Fidelity is the whole point of the tool: say it first, say it plainly.
fn fidelity(out: &mut impl Write, report: &Report, p: &Palette) -> fmt::Result {
    let (onchain, local) = (report.result.onchain.label(), report.result.local.label());
    match report.fidelity.divergence {
        None => writeln!(out, "{}", p.green("✓ replay matches network"))?,
        Some(Divergence::Result) => {
            writeln!(
                out,
                "{}",
                p.yellow(&format!(
                    "⚠ replay diverged from the network ({onchain} vs {local})"
                ))
            )?;
            writeln!(
                out,
                "{}",
                p.dim("  the ledger has almost certainly moved since this transaction ran")
            )?;
        }
        Some(Divergence::Error) => {
            writeln!(
                out,
                "{}",
                p.yellow("⚠ replay diverged from the network (same result, different error)")
            )?;
            writeln!(
                out,
                "{}",
                p.dim("  the replay failed, but not the way the transaction did on-chain")
            )?;
        }
    }
    writeln!(
        out,
        "{}",
        p.dim(&format!("  on-chain  {onchain}\n  local     {local}"))
    )?;
    if !report.footprint.config_drifted.is_empty() {
        writeln!(
            out,
            "{}",
            p.yellow(&format!(
                "⚠ network settings changed since this transaction ran: {}",
                report.footprint.config_drifted.join(", ")
            ))
        )?;
    }
    writeln!(out)
}

fn where_it_broke(
    out: &mut impl Write,
    failure: &Failure,
    p: &Palette,
    detail: Detail,
) -> fmt::Result {
    writeln!(out, "{}", p.bold("where it broke"))?;
    let headline = match &failure.name {
        Some(name) => format!("{} · {name}", failure.description),
        None => failure.description.clone(),
    };
    writeln!(out, "  {}", p.red(&headline))?;
    if !failure.message.is_empty() {
        writeln!(out, "  {}", p.dim(&quoted(&failure.message)))?;
    }
    if let Some(contract) = &failure.contract {
        let location = match &failure.function {
            Some(function) => format!("{contract}::{function}"),
            None => contract.clone(),
        };
        writeln!(
            out,
            "  {}",
            p.dim(&format!("in {location} (depth {})", failure.depth))
        )?;
    }
    if detail >= Detail::Budget {
        if let Some(origin) = &failure.name_origin {
            writeln!(out, "  {}", p.dim(&format!("name from {origin}")))?;
        }
    }
    if detail >= Detail::Diagnostics && failure.trail.len() > 1 {
        writeln!(
            out,
            "  {}",
            p.dim(&format!(
                "{} error diagnostics: {}",
                failure.trail.len(),
                failure.trail.join(" → ")
            ))
        )?;
    }
    writeln!(out)
}

fn call_tree(out: &mut impl Write, calls: &[Call], p: &Palette, detail: Detail) -> fmt::Result {
    writeln!(out, "{}", p.bold("call tree"))?;
    if calls.is_empty() {
        writeln!(
            out,
            "  {}",
            p.dim("no contract was entered before the failure")
        )?;
    }
    for call in calls {
        let indent = match call.depth {
            0 => String::new(),
            depth => format!("{}└─ ", "   ".repeat(depth - 1)),
        };
        let head = match &call.contract {
            Some(contract) => format!("{contract}::{}", call.function),
            None => call.function.clone(),
        };
        let mut line = format!("{indent}{head}({})", call.args.join(", "));

        if detail >= Detail::Budget {
            if let Some(returned) = &call.returned {
                write!(line, " → {returned}")?;
            }
            if call.recovered {
                write!(line, " {}", p.dim("(recovered by try_call)"))?;
            }
        }
        if call.is_failure {
            writeln!(out, "{}", p.red(&format!("{line}  ✗")))?;
        } else if call.open {
            writeln!(out, "{}", p.yellow(&line))?;
        } else {
            writeln!(out, "{line}")?;
        }

        let log_indent = if call.depth == 0 {
            "    · "
        } else {
            "       · "
        };
        for log in &call.logs {
            writeln!(out, "{}{}", p.dim(log_indent), p.dim(log))?;
        }
    }
    writeln!(out)
}

fn budget(out: &mut impl Write, report: &Report, p: &Palette) -> fmt::Result {
    let b = &report.budget;
    writeln!(
        out,
        "{}",
        p.dim(&format!(
            "budget {} / {} cpu instructions · {} / {} memory",
            grouped(b.cpu_insns),
            grouped(b.cpu_limit),
            human_bytes(b.mem_bytes),
            human_bytes(b.mem_limit),
        ))
    )?;
    if let (Some(cpu), Some(mem)) = (b.network_cpu_insns, b.network_mem_bytes) {
        writeln!(
            out,
            "{}",
            p.dim(&format!(
                "network used {} cpu instructions · {} memory",
                grouped(cpu),
                human_bytes(mem)
            ))
        )?;
    }
    Ok(())
}

/// How many drifted entries of each kind the summary lists before it points
/// at `-v`. An actively maintained contract can have dozens.
const DRIFT_LINES: usize = 5;

/// What is known to differ between the ledger now and the ledger then.
fn drift(out: &mut impl Write, report: &Report, p: &Palette, detail: Detail) -> fmt::Result {
    let f = &report.footprint;
    writeln!(
        out,
        "{}",
        p.dim(&format!(
            "footprint {} entries · {} drifted · {} ttl-drifted · {} missing · prng seed {}",
            f.entries,
            f.drifted.len(),
            f.ttl_drifted.len(),
            f.missing,
            match report.prng_seed {
                PrngSeed::Derived => "derived",
                PrngSeed::Fixed => "fixed (apply order unknown)",
            }
        ))
    )?;
    let ttl_drifted: Vec<String> = f
        .ttl_drifted
        .iter()
        .map(|drifted| format!("{} ({})", drifted.entry, drifted.reason.describe()))
        .collect();
    let limit = if detail >= Detail::Budget {
        usize::MAX
    } else {
        DRIFT_LINES
    };
    for (label, entries) in [
        ("archived:", &f.archived),
        ("drifted:", &f.drifted),
        ("ttl drifted:", &ttl_drifted),
    ] {
        for entry in entries.iter().take(limit) {
            writeln!(out, "  {} {entry}", p.yellow(label))?;
        }
        if entries.len() > limit {
            writeln!(
                out,
                "  {}",
                p.dim(&format!(
                    "… and {} more (-v lists them all)",
                    entries.len() - limit
                ))
            )?;
        }
    }
    writeln!(out)
}

fn diagnostics(out: &mut impl Write, report: &Report, p: &Palette) -> fmt::Result {
    for (title, lines) in [
        ("on-chain diagnostics", &report.diagnostics.onchain),
        ("local diagnostics", &report.diagnostics.local),
    ] {
        if lines.is_empty() {
            continue;
        }
        writeln!(out, "{}", p.bold(title))?;
        for line in lines {
            writeln!(out, "  {line}")?;
        }
        writeln!(out)?;
    }
    Ok(())
}

/// A message as a quoted literal, with control characters escaped.
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.escape_debug())
}

/// Seconds since the Unix epoch into a fixed, sortable UTC stamp.
///
/// Decision (hand-rolled, kept): this is Howard Hinnant's `civil_from_days`,
/// fifteen lines of integer arithmetic that are exact for the whole `u64`
/// range a ledger close time can take. A date crate for one line of output
/// is not worth its weight next to the Soroban host; the edge cases are
/// pinned by the tests below instead.
fn utc(timestamp: u64) -> String {
    let days = (timestamp / 86_400) as i64;
    let secs = timestamp % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
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

/// `1234567` → `1,234,567`.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
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
    use crate::replay::Fidelity;
    use crate::report::{TtlDriftEntry, TtlDriftReason};
    use crate::test_support::sample_report;

    #[test]
    fn formats_timestamps() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(1_700_000_000), "2023-11-14 22:13:20 UTC");
    }

    #[test]
    fn timestamps_cross_leap_days_and_year_boundaries() {
        // 2024 is a leap year: Feb 28 → Feb 29 → Mar 1.
        assert_eq!(utc(1_709_164_799), "2024-02-28 23:59:59 UTC");
        assert_eq!(utc(1_709_164_800), "2024-02-29 00:00:00 UTC");
        assert_eq!(utc(1_709_251_200), "2024-03-01 00:00:00 UTC");
        // 2023 is not: Feb 28 → Mar 1.
        assert_eq!(utc(1_677_628_799), "2023-02-28 23:59:59 UTC");
        assert_eq!(utc(1_677_628_800), "2023-03-01 00:00:00 UTC");
        // Year boundary.
        assert_eq!(utc(1_735_689_599), "2024-12-31 23:59:59 UTC");
        assert_eq!(utc(1_735_689_600), "2025-01-01 00:00:00 UTC");
        // Century rules: 2000 was a leap year, 2100 will not be.
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc(4_107_456_000), "2100-02-28 00:00:00 UTC");
        assert_eq!(utc(4_107_542_400), "2100-03-01 00:00:00 UTC");
        // Month ends.
        assert_eq!(utc(1_706_745_599), "2024-01-31 23:59:59 UTC");
        assert_eq!(utc(1_719_791_999), "2024-06-30 23:59:59 UTC");
    }

    #[test]
    fn far_future_timestamps_do_not_panic() {
        assert_eq!(utc(253_402_300_799), "9999-12-31 23:59:59 UTC");
        assert!(utc(u64::MAX).ends_with(" UTC"));
    }

    #[test]
    fn groups_digits() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(4_120), "4,120");
        assert_eq!(grouped(100_000_000), "100,000,000");
        assert_eq!(grouped(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn scales_bytes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(41_943_040), "40.0 MB");
        assert_eq!(human_bytes(u64::MAX), "16777216.0 TB");
    }

    #[test]
    fn quotes_messages() {
        assert_eq!(quoted("plain"), "\"plain\"");
        assert_eq!(quoted("a \"b\"\n"), r#""a \"b\"\n""#);
    }

    #[test]
    fn detail_follows_verbosity() {
        assert_eq!(Detail::from_verbosity(0), Detail::Summary);
        assert_eq!(Detail::from_verbosity(1), Detail::Budget);
        assert_eq!(Detail::from_verbosity(2), Detail::Diagnostics);
        assert_eq!(Detail::from_verbosity(9), Detail::Diagnostics);
    }

    #[test]
    fn colour_is_only_emitted_when_asked_for() {
        let report = sample_report();
        let plain = human(&report, false, Detail::Diagnostics);
        assert!(
            !plain.contains('\u{1b}'),
            "plain output must carry no escapes"
        );

        let coloured = human(&report, true, Detail::Diagnostics);
        assert!(coloured.contains("\u{1b}[32m✓ replay matches network\u{1b}[39m"));
        assert!(coloured.contains("\u{1b}[1mcall tree\u{1b}[0m"));
        // Same text once the escapes are stripped.
        let mut stripped = String::new();
        let mut chars = coloured.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                stripped.push(c);
            }
        }
        assert_eq!(stripped, plain);
    }

    #[test]
    fn summary_narrative() {
        insta::assert_snapshot!(
            "human_summary",
            human(&sample_report(), false, Detail::Summary)
        );
    }

    #[test]
    fn verbose_narrative_adds_budget_and_diagnostics() {
        insta::assert_snapshot!(
            "human_diagnostics",
            human(&sample_report(), false, Detail::Diagnostics)
        );
    }

    #[test]
    fn divergence_explains_itself_without_flags() {
        let mut report = sample_report();
        report.fidelity = Fidelity::diverged(Divergence::Result);
        report.result.local = crate::report::ResultSummary {
            code: "entry_archived",
            result_hash: None,
        };
        report.footprint.archived = vec!["contract code abcd…".to_string()];
        report.footprint.config_drifted = vec!["StateArchival".to_string()];
        report.footprint.ttl_drifted = vec![TtlDriftEntry {
            entry: "contract code ffff…".to_string(),
            reason: TtlDriftReason::ArchivedSince,
        }];
        report.prng_seed = PrngSeed::Fixed;
        report.story = crate::story::Story::default();

        insta::assert_snapshot!("human_diverged", human(&report, false, Detail::Summary));

        report.fidelity = Fidelity::diverged(Divergence::Error);
        let text = human(&report, false, Detail::Summary);
        assert!(text.contains("⚠ replay diverged from the network (same result, different error)"));
    }

    #[test]
    fn json_document() {
        insta::assert_snapshot!("json_document", json(&sample_report()).unwrap());
    }
}
