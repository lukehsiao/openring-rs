use clap::Parser;
use miette::{Context, Result};
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
    // I feel like I shouldn't need wrap_err, but it doesn't work without it.
    openring::run(args).await.wrap_err("runtime error")
}
