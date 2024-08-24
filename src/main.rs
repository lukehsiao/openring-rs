use clap::Parser;
use miette::{Context, Result};
use std::io;

use openring::{self, args::Args};

fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(format!(
            "openring={},html5ever=off,ureq=off",
            convert_filter(args.verbose.log_level_filter())
        ))
        .with_writer(io::stderr)
        .init();
    // I feel like I shouldn't need wrap_err, but it doesn't work without it.
    openring::run(args).wrap_err("runtime error")
}

fn convert_filter(filter: log::LevelFilter) -> tracing_subscriber::filter::LevelFilter {
    match filter {
        log::LevelFilter::Off => tracing_subscriber::filter::LevelFilter::OFF,
        log::LevelFilter::Error => tracing_subscriber::filter::LevelFilter::ERROR,
        log::LevelFilter::Warn => tracing_subscriber::filter::LevelFilter::WARN,
        log::LevelFilter::Info => tracing_subscriber::filter::LevelFilter::INFO,
        log::LevelFilter::Debug => tracing_subscriber::filter::LevelFilter::DEBUG,
        log::LevelFilter::Trace => tracing_subscriber::filter::LevelFilter::TRACE,
    }
}
