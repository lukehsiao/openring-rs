use clap::Parser;
use miette::{IntoDiagnostic, Result};
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
    openring::run(args).await.into_diagnostic()
}
