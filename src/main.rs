use std::io::IsTerminal;

use clap::Parser;
use miette::Result;
use tracing_log::AsTrace;

use openring::{self, args::Args, terminal::Terminal};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let terminal = Terminal::create();

    let log_terminal = terminal.clone();
    let logs = tracing_subscriber::fmt()
        .with_env_filter(format!(
            "openring={},html5ever=off",
            args.verbose.log_level_filter().as_trace()
        ))
        // Suspends any active progress bar around each log line, so the two
        // don't splice on a tty.
        .with_writer(move || log_terminal.log_writer());
    if std::io::stderr().is_terminal() {
        // A person reads these lines beside the progress list. A timestamp
        // and module path make each line wrap and add nothing for them.
        // ANSI stays at the default, so NO_COLOR still turns color off.
        logs.without_time().with_target(false).compact().init();
    } else {
        // These lines go to a file or a CI log. Escape codes are noise
        // there, and timestamps help when someone reads the log later.
        logs.with_ansi(false).init();
    }
    // `?` converts `OpenringError` into a `miette::Report` via its `Diagnostic`
    // impl, so the `#[diagnostic(code(..))]` codes render (unlike `into_diagnostic`,
    // which discards them).
    openring::run(args, &terminal, std::io::stdout()).await?;
    Ok(())
}
