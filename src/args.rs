use std::{path::PathBuf, time::Duration};

use clap::{builder::ValueHint, Parser};
use clap_verbosity_flag::Verbosity;
use jiff::civil::Date;
use url::Url;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Total number of articles to fetch
    #[arg(short, long, default_value_t = 3)]
    pub num_articles: usize,
    /// Number of most recent articles to get from each feed
    #[arg(short, long, default_value_t = 1)]
    pub per_source: usize,
    /// File with URLs of Atom/RSS feeds to read (one URL per line, lines starting with '#' or "//" are ignored)
    #[arg(short = 'S', long, value_name = "FILE", value_hint=ValueHint::FilePath)]
    pub url_file: Option<PathBuf>,
    /// Tera template file
    #[arg(short, long, value_parser, value_name = "FILE", value_hint=ValueHint::FilePath)]
    pub template_file: PathBuf,
    /// A single URL to consider (can be repeated to specify multiple)
    #[arg(short = 's', long, value_hint=ValueHint::Url)]
    pub url: Vec<Url>,
    /// Only include articles before this date (in YYYY-MM-DD format).
    ///
    /// This is naive (no timezone), so articles close to the boundary in different timezones might
    /// be unexpectedly filtered. In addition, some feeds are truncated, and may have already pruned
    /// away articles before this date from the feed itself.
    #[arg(short, long)]
    pub before: Option<Date>,
    /// Use request cache stored on disk at `.openringcache`
    ///
    /// Note that this only prevents refetching if the feed source responds
    /// with a 429. In this case, we respect Retry-After, or default to 4h.
    /// Otherwise, the existence of a cache file just allows openring to respect
    /// ETag and Last-Modified headers for conditional requests.
    #[arg(short, long)]
    pub cache: bool,
    /// Discard all cached requests older than this duration
    #[arg(
        long,
        value_parser = humantime::parse_duration,
        default_value = "14d"
    )]
    pub max_cache_age: Duration,
    #[clap(flatten)]
    pub verbose: Verbosity,
}

#[cfg(test)]
mod test {
    use crate::*;
    #[test]
    fn verify_app() {
        use clap::CommandFactory;
        Args::command().debug_assert()
    }
}
