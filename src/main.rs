use clap::Parser;
use figment::{
    Figment,
    providers::{Format, Serialized, Toml},
};
use miette::{Context, IntoDiagnostic, Result};
use miette::{IntoDiagnostic, Result};
use std::io;
use tracing_log::AsTrace;

use openring::{
    self,
    config::{Config, get_config_path},
};

#[tokio::main]
async fn main() -> Result<()> {
    let config: Config = {
        if let Some(config_path) = get_config_path() {
            // Parse CLI arguments. Override CLI config values with those in
            // openring/config.toml
            Figment::new()
                .merge(Serialized::defaults(Config::parse()))
                .merge(Toml::file(config_path))
                .extract()
                .into_diagnostic()?
        } else {
            Figment::new()
                .merge(Serialized::defaults(Config::parse()))
                .extract()
                .into_diagnostic()?
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(format!(
            "openring={},html5ever=off",
            config.verbose.log_level_filter().as_trace()
        ))
        .with_writer(io::stderr)
        .init();
    openring::run(config).await.into_diagnostic()
}
