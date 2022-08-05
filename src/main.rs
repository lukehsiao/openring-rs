use std::path::PathBuf;

use clap::{builder::ValueHint, Parser};
use url::Url;

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
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

fn main() {
    let args = Args::parse();

    dbg!(args);
    println!("Hello, world!");
}
