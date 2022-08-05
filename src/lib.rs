use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{builder::ValueHint, Parser};
use log::debug;
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
    /// A specific URL to consider. Can be repeated.
    #[clap(short = 's', long, value_parser, value_hint=ValueHint::Url)]
    urls: Vec<Url>,
}

pub fn run(args: Args) -> Result<()> {
    debug!("Args: {:#?}", args);

    let mut urls = args.urls;

    // Grab all urls from the provided file
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

    Ok(())
}
