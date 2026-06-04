use clap::Parser;
use miette::Result;
use std::io;
use tracing_log::AsTrace;

use openring::{self, args::Args};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(format!(
            "openring={},html5ever=off",
            args.verbose.log_level_filter().as_trace()
        ))
        .with_writer(io::stderr)
        .init();
    // `?` converts `OpenringError` into a `miette::Report` via its `Diagnostic`
    // impl, so the `#[diagnostic(code(..))]` codes render (unlike `into_diagnostic`,
    // which discards them).
    openring::run(args).await?;
    Ok(())
}
