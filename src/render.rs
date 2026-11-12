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
    /// Map the number of `-v` flags to a level; more than two means
    /// [`Detail::Diagnostics`].
    pub fn from_verbosity(verbose: u8) -> Detail {
        match verbose {
            0 => Detail::Summary,
            1 => Detail::Budget,
            _ => Detail::Diagnostics,
        }
    }
}

/// When to colour the narrative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ColorChoice {
    /// Colour when stdout is a terminal that can show it.
    #[default]
    Auto,
    /// Colour even when stdout is not a terminal.
    Always,
    /// Never colour.
    Never,
}

/// The environment variables and the terminal check [`use_color`] consults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColorEnv {
    /// `NO_COLOR`, if set.
    pub no_color: Option<std::ffi::OsString>,
    /// `CLICOLOR_FORCE`, if set.
    pub clicolor_force: Option<std::ffi::OsString>,
    /// `TERM`, if set.
    pub term: Option<std::ffi::OsString>,
    /// Whether stdout is a terminal.
    pub stdout_is_terminal: bool,
}

impl ColorEnv {
    /// Read the variables and the terminal check from this process.
    pub fn current() -> Self {
        Self {
            no_color: std::env::var_os("NO_COLOR"),
            clicolor_force: std::env::var_os("CLICOLOR_FORCE"),
            term: std::env::var_os("TERM"),
            stdout_is_terminal: std::io::stdout().is_terminal(),
        }
    }
}

/// Should ANSI colour be emitted on stdout?
///
/// `--color always|never` wins. Otherwise a non-empty `NO_COLOR` turns colour
/// off (<https://no-color.org>: an empty value does not count), a
/// `CLICOLOR_FORCE` other than empty or `0` turns it on, and failing both it
/// is on for a terminal that is not `TERM=dumb`.
pub fn use_color(choice: ColorChoice, env: &ColorEnv) -> bool {
    let set = |value: &Option<std::ffi::OsString>| value.as_ref().is_some_and(|v| !v.is_empty());
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto if set(&env.no_color) => false,
        ColorChoice::Auto if set(&env.clicolor_force) => {
            env.clicolor_force.as_deref() != Some(std::ffi::OsStr::new("0"))
        }
        ColorChoice::Auto => {
            env.stdout_is_terminal && env.term.as_deref() != Some(std::ffi::OsStr::new("dumb"))
        }
    }
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

/// The JSON document of a run that failed: `{"schemaVersion": 1, "error":
/// {"kind": "not_found", "message": "…"}}`. `kind` is stable per kind of
/// failure; `message` is the text printed on stderr.
pub fn json_error(error: &crate::RewindError) -> String {
    let document = serde_json::json!({
        "schemaVersion": crate::report::SCHEMA_VERSION,
        "error": { "kind": error.kind(), "message": error.to_string() },
    });
    // A map of strings always serialises.
    serde_json::to_string_pretty(&document).unwrap_or_default()
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
        p.bold(&safe(&report.network)),
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
    if !report.footprint.config_drift_unknown.is_empty() {
        writeln!(
            out,
            "{}",
            p.yellow(&format!(
                "⚠ the RPC does not say when these network settings last changed: {}",
                report.footprint.config_drift_unknown.join(", ")
            ))
        )?;
    }
    if !report.footprint.archived_inferred.is_empty() {
        writeln!(
            out,
            "{}",
            p.dim(
                "  the archived entry's TTL at the time is lost; that it was archived is taken \
                 from the network's result, not reproduced"
            )
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
    let located = |out: &mut dyn Write,
                   headline: String,
                   message: &str,
                   contract: &Option<String>,
                   function: &Option<String>,
                   depth: Option<usize>|
     -> fmt::Result {
        writeln!(out, "  {}", p.red(&headline))?;
        if !message.is_empty() {
            writeln!(out, "  {}", p.dim(&quoted(message)))?;
        }
        if let Some(contract) = contract {
            let location = match function {
                Some(function) => format!("{}::{}", safe(contract), safe(function)),
                None => safe(contract),
            };
            let depth = depth.map_or(String::new(), |depth| format!(" (depth {depth})"));
            writeln!(out, "  {}", p.dim(&format!("in {location}{depth}")))?;
        }
        Ok(())
    };
    let named = |description: &str, name: &Option<String>| match name {
        Some(name) => format!("{description} · {}", safe(name)),
        None => description.to_string(),
    };

    located(
        out,
        named(&failure.description, &failure.name),
        &failure.message,
        &failure.contract,
        &failure.function,
        failure.depth,
    )?;
    if let Some(cause) = &failure.cause {
        located(
            out,
            format!("caused by {}", named(&cause.description, &cause.name)),
            &cause.message,
            &cause.contract,
            &cause.function,
            cause.depth,
        )?;
    }
    if detail >= Detail::Budget {
        let origin = failure
            .name_origin
            .as_ref()
            .or(failure.cause.as_ref().and_then(|c| c.name_origin.as_ref()));
        if let Some(origin) = origin {
            writeln!(out, "  {}", p.dim(&format!("name from {}", safe(origin))))?;
        }
    }
    if detail >= Detail::Diagnostics && failure.trail.len() > 1 {
        let trail: Vec<String> = failure.trail.iter().map(|step| safe(step)).collect();
        writeln!(
            out,
            "  {}",
            p.dim(&format!(
                "{} error diagnostics: {}",
                trail.len(),
                trail.join(" → ")
            ))
        )?;
    }
    writeln!(out)
}

/// The call tree, drawn with real connectors. A frame's state is spelled out
/// in words as well as colour, so the tree reads the same in a pipe:
///
/// - `✗` the frame the error was raised in;
/// - `(no return)` a frame that never returned: it is on the failure path;
/// - `(recovered by try_call)` a frame that failed while its caller carried on.
fn call_tree(out: &mut impl Write, calls: &[Call], p: &Palette, detail: Detail) -> fmt::Result {
    writeln!(out, "{}", p.bold("call tree"))?;
    if calls.is_empty() {
        writeln!(
            out,
            "  {}",
            p.dim("no contract was entered before the failure")
        )?;
    }
    // Whether each frame is the last child of its parent.
    let last_child: Vec<bool> = (0..calls.len())
        .map(|i| {
            calls[i + 1..]
                .iter()
                .find(|later| later.depth <= calls[i].depth)
                .is_none_or(|later| later.depth < calls[i].depth)
        })
        .collect();
    // For the frame being drawn: per ancestor level, whether that ancestor
    // still has siblings to come (and so needs its rail drawn).
    let mut rails: Vec<bool> = Vec::new();

    for (i, call) in calls.iter().enumerate() {
        rails.truncate(call.depth.saturating_sub(1));
        let ancestry: String = rails
            .iter()
            .map(|open| if *open { "│  " } else { "   " })
            .collect();
        let branch = match (call.depth, last_child[i]) {
            (0, _) => "",
            (_, true) => "└─ ",
            (_, false) => "├─ ",
        };
        let head = match &call.contract {
            Some(contract) => format!("{}::{}", safe(contract), safe(&call.function)),
            None => safe(&call.function),
        };
        let args: Vec<String> = call.args.iter().map(|arg| safe(arg)).collect();
        let mut line = format!("{ancestry}{branch}{head}({})", args.join(", "));

        if detail >= Detail::Budget
            && let Some(returned) = &call.returned
        {
            write!(line, " → {}", safe(returned))?;
        }
        // Each part is painted on its own: a style's reset would end the
        // colour of anything it were nested in.
        let recovered = if call.recovered {
            format!("  {}", p.dim("(recovered by try_call)"))
        } else {
            String::new()
        };
        if call.is_failure {
            writeln!(out, "{}{recovered}{}", p.red(&line), p.red("  ✗"))?;
        } else if call.open {
            writeln!(
                out,
                "{}{recovered}{}",
                p.yellow(&line),
                p.yellow("  (no return)")
            )?;
        } else {
            writeln!(out, "{line}{recovered}")?;
        }

        if call.depth > 0 {
            rails.push(!last_child[i]);
        }
        // Logs hang under their frame, clear of its children's connectors.
        let below: String = rails
            .iter()
            .map(|open| if *open { "│  " } else { "   " })
            .collect();
        for log in &call.logs {
            writeln!(out, "{}", p.dim(&format!("{below}    · {}", safe(log))))?;
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
            "footprint {} entries · {} drifted · {} ttl-drifted · {} missing{} · prng seed {}",
            f.entries,
            f.drifted.len(),
            f.ttl_drifted.len(),
            f.missing,
            if f.rebuilt.is_empty() {
                String::new()
            } else {
                format!(" · {} rebuilt", f.rebuilt.len())
            },
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
        ("drift unknown:", &f.drift_unknown),
        ("extension missing:", &f.ext_missing),
    ] {
        for entry in entries.iter().take(limit) {
            writeln!(out, "  {} {}", p.yellow(label), safe(entry))?;
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
    if let Some(note) = &f.rebuild_note {
        writeln!(out, "  {}", p.yellow(note))?;
    }
    // What was rebuilt is the replay working as intended: detail, not a warning.
    if detail >= Detail::Budget {
        for entry in &f.rebuilt {
            writeln!(
                out,
                "  {} {}",
                p.dim("rebuilt from ledger metadata:"),
                safe(entry)
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
            writeln!(out, "  {}", safe(line))?;
        }
        writeln!(out)?;
    }
    Ok(())
}

/// A message as a quoted literal, with control characters escaped.
fn quoted(text: &str) -> String {
    format!("\"{}\"", safe(text).replace('"', "\\\""))
}

/// Text that came from a contract, an RPC or a file, made safe to print.
///
/// Log strings, error-enum names, function names and custom network names are
/// arbitrary bytes chosen by someone else. Control characters (ESC, CR, LF,
/// …) and the Unicode characters that reorder or hide text would let them
/// move the cursor, recolour the terminal or forge lines of this report, so
/// they are shown as escapes instead.
fn safe(text: &str) -> String {
    let hostile = |c: char| {
        c.is_control()
            || matches!(
                c,
                // Soft hyphen, Arabic letter mark, Mongolian vowel separator.
                '\u{ad}' | '\u{61c}' | '\u{180e}'
                    // Zero-width characters and directional marks.
                    | '\u{200b}'..='\u{200f}'
                    // Line and paragraph separators, directional embeddings.
                    | '\u{2028}'..='\u{202e}'
                    // Word joiner, invisible operators, directional isolates.
                    | '\u{2060}'..='\u{2064}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{feff}'
                    // Tag characters.
                    | '\u{e0000}'..='\u{e007f}'
            )
    };
    if !text.contains(hostile) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if hostile(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
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
    fn untrusted_text_cannot_drive_the_terminal() {
        assert_eq!(safe("plain · text → ok"), "plain · text → ok");
        assert_eq!(safe("a\u{1b}[31mb\r\nc"), "a\\u{1b}[31mb\\r\\nc");
        // Characters that reorder or hide text are spelled out too.
        assert_eq!(safe("x\u{202e}y\u{200b}z"), "x\\u{202e}y\\u{200b}z");
        for hidden in [
            '\u{ad}',
            '\u{61c}',
            '\u{180e}',
            '\u{2060}',
            '\u{2064}',
            '\u{2066}',
            '\u{2069}',
            '\u{e0000}',
            '\u{e0041}',
            '\u{e007f}',
        ] {
            let text = format!("a{hidden}b");
            assert_eq!(safe(&text), format!("a{}b", hidden.escape_default()));
        }
        // Their neighbours are left alone.
        for visible in ['\u{ae}', '\u{2065}', '\u{e0080}'] {
            let text = format!("a{visible}b");
            assert_eq!(safe(&text), text);
        }

        // Everything a contract, an RPC or a file can put in the report.
        let hostile = "evil\u{1b}[2J\rforged line\n";
        let mut report = sample_report();
        report.network = hostile.to_string();
        report.story.calls[0].function = hostile.to_string();
        report.story.calls[0].args = vec![hostile.to_string()];
        report.story.calls[0].logs = vec![hostile.to_string()];
        report.story.calls[1].returned = Some(hostile.to_string());
        if let Some(failure) = &mut report.story.failure {
            failure.name = Some(hostile.to_string());
            failure.name_origin = Some(hostile.to_string());
            failure.message = hostile.to_string();
            failure.function = Some(hostile.to_string());
            failure.trail = vec![hostile.to_string(), hostile.to_string()];
            failure.cause = Some(crate::story::Cause {
                contract_code: 1,
                description: "contract error #1".to_string(),
                name: Some(hostile.to_string()),
                name_origin: None,
                message: hostile.to_string(),
                contract: Some(hostile.to_string()),
                function: Some(hostile.to_string()),
                depth: Some(1),
            });
        }
        report.footprint.drifted = vec![hostile.to_string()];
        report.footprint.rebuilt = vec![hostile.to_string()];
        report.footprint.rebuild_note = None;
        report.diagnostics.onchain = vec![hostile.to_string()];

        let text = human(&report, false, Detail::Diagnostics);
        assert!(!text.contains('\u{1b}') && !text.contains('\r'), "{text:?}");
        // No line of the report starts with text the contract chose.
        assert!(
            !text.lines().any(|line| line.starts_with("forged")),
            "{text}"
        );
        assert!(text.contains("evil\\u{1b}[2J\\rforged line\\n"));
    }

    #[test]
    fn frames_are_told_apart_without_colour() {
        let call = |depth, function: &str| Call {
            depth,
            contract: None,
            function: function.to_string(),
            args: Vec::new(),
            args_xdr: Vec::new(),
            returned: None,
            returned_xdr: None,
            open: false,
            recovered: false,
            is_failure: false,
            logs: Vec::new(),
        };
        let mut calls = vec![
            call(0, "root"),
            call(1, "first"),
            call(2, "leaf"),
            call(1, "probe"),
            call(1, "last"),
            call(2, "deep"),
            call(3, "deeper"),
        ];
        calls[0].open = true;
        calls[1].returned = Some("1".to_string());
        calls[2].logs = vec!["from the leaf".to_string()];
        calls[3].recovered = true;
        calls[4].open = true;
        calls[5].open = true;
        calls[5].logs = vec!["about to fail".to_string()];
        calls[6].is_failure = true;
        calls[6].open = true;

        let mut out = String::new();
        call_tree(&mut out, &calls, &Palette { on: false }, Detail::Summary).unwrap();
        assert_eq!(
            out,
            "call tree\n\
             root()  (no return)\n\
             ├─ first()\n\
             │  └─ leaf()\n\
             │         · from the leaf\n\
             ├─ probe()  (recovered by try_call)\n\
             └─ last()  (no return)\n\
             \x20  └─ deep()  (no return)\n\
             \x20         · about to fail\n\
             \x20     └─ deeper()  ✗\n\n"
        );
    }

    #[test]
    fn colour_follows_the_flag_then_the_environment() {
        use std::ffi::OsString;
        let env = |no_color: Option<&str>, force: Option<&str>, term: Option<&str>, tty| ColorEnv {
            no_color: no_color.map(OsString::from),
            clicolor_force: force.map(OsString::from),
            term: term.map(OsString::from),
            stdout_is_terminal: tty,
        };
        let auto = |env: &ColorEnv| use_color(ColorChoice::Auto, env);

        assert!(auto(&env(None, None, Some("xterm"), true)));
        assert!(!auto(&env(None, None, Some("xterm"), false)));
        assert!(!auto(&env(None, None, Some("dumb"), true)));
        // NO_COLOR counts only when it is not empty.
        assert!(!auto(&env(Some("1"), None, None, true)));
        assert!(auto(&env(Some(""), None, None, true)));
        // CLICOLOR_FORCE colours a pipe; NO_COLOR still wins.
        assert!(auto(&env(None, Some("1"), None, false)));
        assert!(!auto(&env(None, Some("0"), None, false)));
        assert!(!auto(&env(None, Some(""), None, false)));
        assert!(!auto(&env(Some("1"), Some("1"), None, true)));
        // The flag beats everything.
        assert!(use_color(
            ColorChoice::Always,
            &env(Some("1"), None, Some("dumb"), false)
        ));
        assert!(!use_color(
            ColorChoice::Never,
            &env(None, Some("1"), None, true)
        ));
    }

    #[test]
    fn errors_have_a_json_document() {
        let error = crate::RewindError::LedgerUnavailable { sequence: 7 };
        let document: serde_json::Value = serde_json::from_str(&json_error(&error)).unwrap();
        assert_eq!(document["schemaVersion"], 1);
        assert_eq!(document["error"]["kind"], "ledger_unavailable");
        assert_eq!(document["error"]["message"], error.to_string());
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
