use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{serde::ts_seconds, DateTime, Utc};
use clap::{builder::ValueHint, Parser};
use log::{debug, info};
use serde::Serialize;
use tera::Tera;
use url::Url;

#[derive(Parser, Debug)]
#[clap(author, version, about)]
pub struct Args {
    /// Total number of articles to fetch
    #[clap(short, long, value_parser, default_value_t = 3)]
    num_feed: u8,
    /// Number of most recent articles to get from each feed
    #[clap(short, long, value_parser, default_value_t = 1)]
    per_source: u8,
    /// Length (in chars) of the article summaries.
    #[clap(short = 'l', long, value_parser, default_value_t = 256)]
    summary_len: u32,
    /// File with URLs of RSS feeds to read.
    #[clap(short = 'S', long, value_parser, value_name = "FILE", value_hint=ValueHint::FilePath)]
    url_file: Option<PathBuf>,
    /// Tera template file.
    #[clap(short, long, value_parser, value_name = "FILE", value_hint=ValueHint::FilePath)]
    template_file: PathBuf,
    /// A specific URL to consider. Can be repeated.
    #[clap(short = 's', long, value_parser, value_hint=ValueHint::Url)]
    urls: Vec<Url>,
}

#[derive(Serialize, Debug)]
pub struct Article {
    link: Url,
    title: String,
    summary: String,
    source_link: Url,
    source_title: String,
    #[serde(with = "ts_seconds")]
    date: DateTime<Utc>,
}

pub fn run(args: Args) -> Result<()> {
    debug!("Args: {:#?}", args);
    let mut urls = args.urls;

    if let Some(file) = args.url_file {
        let file = File::open(file)?;
        let reader = BufReader::new(file);

        let mut file_urls: Vec<Url> = reader
            .lines()
            .map(|s| s.expect("Unable to parse line."))
            .map(|l| Url::parse(&l).expect("Unable to parse url"))
            .collect();
        urls.append(&mut file_urls);
    };
    debug!("Fetching these urls: {:#?}", urls);

    let template = fs::read_to_string(&args.template_file)
        .with_context(|| format!("Failed to read file `{:?}`", args.template_file))?;
    let mut context = tera::Context::new();

    info!("Fetching feeds...");

    // TODO: this validation of the template should come before all the time spent fetching feeds.
    let output = Tera::one_off(&template, &context, true)
        .with_context(|| format!("Failed to parse Tera template:\n{}", template))?;
    println!("{output}");
    Ok(())
}
