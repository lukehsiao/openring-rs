use anyhow::Result;

use clap::Parser;

use openring::{self, Args};

fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(convert_filter(args.verbose.log_level_filter()))
        .init();

    openring::run(args)
}

fn convert_filter(filter: log::LevelFilter) -> String {
    let filter = match filter {
        log::LevelFilter::Off => "off",
        log::LevelFilter::Error => "error",
        log::LevelFilter::Warn => "warn",
        log::LevelFilter::Info => "info",
        log::LevelFilter::Debug => "debug",
        log::LevelFilter::Trace => "trace",
    };
    format!("openring={},html5ever=off,ureq=off", filter)
}
