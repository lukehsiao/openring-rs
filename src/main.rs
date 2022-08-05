use anyhow::Result;

use clap::Parser;
use openring::{self, Args};

fn main() -> Result<()> {
    // Set the RUST_LOG, if it hasn't been explicitly defined
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "openring=warn,html5ever=off")
    }
    env_logger::init();
    let args = Args::parse();

    openring::run(args)
}
