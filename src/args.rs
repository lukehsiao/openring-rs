use std::{path::PathBuf, time::Duration};

use clap::{Parser, builder::ValueHint};
use clap_verbosity_flag::{Verbosity, WarnLevel};
use jiff::civil::Date;

const AFTER_LONG_HELP: &str = "\
Examples:
  Render the three most recent articles across a blogroll:
      openring -S urls.txt -t in.html > webring.html

  Down-weight a daily blog so it competes like a weekly one (urls.txt):
      https://quiet.example/feed.xml
      https://daily.example/feed.xml 7

  The same weight syntax works inline:
      openring -s 'https://daily.example/feed.xml 7' -t in.html
";

#[derive(Parser, Debug, Default)]
#[command(author, version, about, long_about = None, after_long_help = AFTER_LONG_HELP)]
pub struct Args {
    /// Total number of articles to fetch
    #[arg(short, long, default_value_t = 3)]
    pub num_articles: usize,
    /// Number of most recent articles to get from each feed
    #[arg(short, long, default_value_t = 1)]
    pub per_source: usize,
    /// File with URLs of Atom/RSS feeds to read (one URL per line, optionally followed by an integer weight; see --help)
    ///
    /// Each line is `URL [WEIGHT]`; blank lines and lines starting with '#' or "//" are ignored.
    /// A feed with weight N contributes a random pick from its N newest articles instead of
    /// always its newest, which keeps prolific feeds from dominating the ring: weight 7 roughly
    /// treats a daily blog like a weekly one. A feed with fewer than N recent articles sits out
    /// proportionally often instead. Listing one feed with two different weights is an error.
    #[arg(short = 'S', long, value_name = "FILE", value_hint=ValueHint::FilePath)]
    pub url_file: Option<PathBuf>,
    /// Tera template file
    #[arg(short, long, value_parser, value_name = "FILE", value_hint=ValueHint::FilePath)]
    pub template_file: PathBuf,
    /// A single URL to consider, optionally followed by a weight, e.g. `https://example.com/feed.xml 7` (can be repeated to specify multiple)
    ///
    /// Accepts the same `URL [WEIGHT]` syntax as the urls file; see --url-file for what
    /// weights do.
    // Raw strings on purpose: parsing happens in FeedSet::resolve, where errors carry
    // span diagnostics pointing at the offending token. clap's error channel cannot
    // transport a miette report.
    #[arg(short = 's', long, value_hint=ValueHint::Url)]
    pub url: Vec<String>,
    /// Only include articles before this date (in YYYY-MM-DD format).
    ///
    /// This is naive (no timezone), so articles close to the boundary in different timezones might
    /// be unexpectedly filtered. In addition, some feeds are truncated, and may have already pruned
    /// away articles before this date from the feed itself.
    #[arg(short, long)]
    pub before: Option<Date>,
    /// Do NOT use request cache stored on disk.
    ///
    /// Note that the cache only prevents refetching if the feed source responds
    /// with a 429. In this case, we respect Retry-After, or default to 4h.
    /// Otherwise, the existence of a cache file just allows openring to respect
    /// `ETag` and `Last-Modified` headers for conditional requests.
    #[arg(long)]
    pub no_cache: bool,
    /// Discard all cached requests older than this duration
    #[arg(
        long,
        value_parser = humantime::parse_duration,
        default_value = "30d"
    )]
    pub max_cache_age: Duration,
    /// Seed the random selection used by weighted feeds, for reproducible output
    ///
    /// Has no effect unless at least one feed has a weight. By default every run draws fresh
    /// entropy so weighted feeds rotate; a fixed seed (e.g. --seed "$(date +%Y%m%d)") keeps
    /// the output stable for a period of your choosing.
    #[arg(long, value_name = "U64")]
    pub seed: Option<u64>,
    // WarnLevel: warnings are actionable (skipped entries, cache failures,
    // redirected feeds) and must not require -v; -q silences them.
    #[clap(flatten)]
    pub verbose: Verbosity<WarnLevel>,
}

#[cfg(test)]
mod test {
    use crate::*;
    #[test]
    fn verify_app() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }

    #[test]
    fn url_arg_passes_inline_weights_through_to_run() {
        use clap::Parser;
        // clap must not reject the `URL WEIGHT` shape; semantic validation
        // happens in run() where errors can carry span diagnostics.
        let args = Args::try_parse_from([
            "openring",
            "--template-file",
            "t.html",
            "--url",
            "https://example.com/feed.xml 7",
        ])
        .unwrap();
        assert_eq!(args.url, ["https://example.com/feed.xml 7"]);
        assert_eq!(args.seed, None);
    }

    #[test]
    fn warnings_are_visible_by_default() {
        use tracing_log::AsTrace;
        // Warnings (skipped entries, cache failures, redirected feeds) are
        // actionable and must not require -v to see.
        assert_eq!(
            Args::default().verbose.log_level_filter().as_trace(),
            tracing::level_filters::LevelFilter::WARN
        );
    }
}
