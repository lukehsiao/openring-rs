use anyhow::Result;

use clap::Parser;
use openring::{self, Args};

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    openring::run(args)
}
