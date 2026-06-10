use clap::Parser;
use miette::Result;
use tracing_log::AsTrace;

use openring::{self, args::Args, progress::SuspendingStderr};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(format!(
            "openring={},html5ever=off",
            args.verbose.log_level_filter().as_trace()
        ))
        // Suspends any active progress bar around each log line, so the two
        // don't splice on a tty.
        .with_writer(|| SuspendingStderr)
        .init();
    // `?` converts `OpenringError` into a `miette::Report` via its `Diagnostic`
    // impl, so the `#[diagnostic(code(..))]` codes render (unlike `into_diagnostic`,
    // which discards them).
    openring::run(args).await?;
    Ok(())
}
