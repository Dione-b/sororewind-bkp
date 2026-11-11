//! The `sororewind` binary: parse arguments, run the pipeline, print the
//! report, exit with the outcome. Everything else lives in the library.

use std::process::ExitCode;

use clap::error::ErrorKind;
use sororewind::cli::Cli;
use sororewind::render::{self, Detail};
use sororewind::{Report, Result};

/// Diagnostics go to stderr, tagged with where they came from: `[rpc] → …`.
/// This is the only place in the program that writes to stderr.
struct StderrLogger;

/// The tags sororewind logs under. Dependencies log too; they are not ours to
/// print.
const TARGETS: [&str; 5] = ["rpc", "fixture", "record", "spec", "replay"];

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        TARGETS.contains(&metadata.target())
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("[{}] {}", record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

fn init_logging(verbose: u8) {
    static LOGGER: StderrLogger = StderrLogger;
    // Installing a logger only fails if one is already set; there is not.
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(match verbose {
        0 => log::LevelFilter::Warn,
        1 => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    });
}

async fn run(cli: &Cli) -> Result<Report> {
    let (rpc, request) = cli.connect().await?;
    sororewind::run(&rpc, &request).await
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse_args(std::env::args_os()) {
        Ok(cli) => cli,
        Err(err) => {
            // `--help` and `--version` are not failures. Everything else is a
            // tool error: clap's default exit code for usage errors is 2,
            // which here means "the replay diverged".
            let _ = err.print();
            return match err.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => ExitCode::SUCCESS,
                _ => ExitCode::from(sororewind::EXIT_ERROR),
            };
        }
    };
    init_logging(cli.verbose);

    let result = run(&cli).await;
    let code = sororewind::exit_code(&result);
    let printed = result.and_then(|report| {
        let text = if cli.json {
            let mut document = render::json(&report)
                .map_err(|e| sororewind::RewindError::Output(e.to_string()))?;
            document.push('\n');
            document
        } else {
            let detail = Detail::from_verbosity(cli.verbose);
            let color = render::use_color(cli.color_choice(), &render::ColorEnv::current());
            render::human(&report, color, detail)
        };
        write_stdout(&text)
    });

    match printed {
        Ok(()) => ExitCode::from(code),
        Err(err) => {
            // A script reading `--json` gets a document either way.
            if cli.json {
                let _ = write_stdout(&format!("{}\n", render::json_error(&err)));
            }
            eprintln!("sororewind: {err}");
            ExitCode::from(sororewind::EXIT_ERROR)
        }
    }
}

/// Write to stdout without panicking when the reader has gone away:
/// `sororewind … | head -1` closes the pipe early, and that is not a failure.
fn write_stdout(text: &str) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(sororewind::RewindError::Output(e.to_string())),
    }
}
