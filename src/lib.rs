use std::{
    cmp::Ordering,
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    result,
    sync::Arc,
    time::Duration,
};

use clap::{builder::ValueHint, crate_name, crate_version, Parser};
use clap_verbosity_flag::Verbosity;
use dashmap::DashMap;
use feed_rs::{model::Feed, parser};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use jiff::{civil::Date, tz::TimeZone, Span, Timestamp, ToSpan};
use miette::{Diagnostic, NamedSource, SourceSpan};
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use serde::{Deserialize, Serialize};
use tera::Tera;
use thiserror::Error;
use tracing::{debug, info, warn};
use ureq::{Agent, AgentBuilder};
use url::{ParseError, Url};

type Result<T> = result::Result<T, OpenringError>;

const OPENRING_CACHE_FILE: &str = ".openringcache";

#[derive(Error, Debug, Diagnostic)]
pub enum OpenringError {
    #[error("No valid published or updated date found.")]
    DateError,
    #[error("No feed urls were provided. Provide feeds with -s or -S <FILE>.")]
    FeedMissing,
    #[error("The feed at `{0}` has a bad a title (e.g., missing link or title).")]
    #[diagnostic(code(openring::feed_title_error))]
    FeedBadTitle(String),
    #[error("Failed to parse civil date.")]
    CivilDateError(#[from] jiff::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    ChronoError(#[from] ChronoError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    FeedUrlError(#[from] FeedUrlError),
    #[error("Failed to open file.")]
    #[diagnostic(code(openring::io_error))]
    IoError(#[from] std::io::Error),
    #[error("Failed to parse URL.")]
    #[diagnostic(code(openring::url_parse_error))]
    UrlParseError(#[from] url::ParseError),
    #[error("Failed to parse tera template.")]
    #[diagnostic(code(openring::template_error))]
    TemplateError(#[from] tera::Error),
    #[error("Invalid cache file found.")]
    #[diagnostic(code(openring::cache_error))]
    CsvError(#[from] csv::Error),
    #[error("Invalid cache file found.")]
    #[diagnostic(code(openring::cache_error))]
    TryFromIntError(#[from] std::num::TryFromIntError),
}

#[derive(Error, Diagnostic, Debug)]
#[error("Failed to parse datetime.")]
#[diagnostic(code(openring::chrono_error))]
pub struct ChronoError {
    #[source_code]
    pub src: NamedSource<String>,
    #[label("this date is invalid")]
    pub span: SourceSpan,
    #[help]
    pub help: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("Failed to parse feed url.")]
#[diagnostic(code(openring::url_parse_error))]
pub struct FeedUrlError {
    #[source_code]
    pub src: NamedSource<String>,
    #[label("this url is invalid")]
    pub span: SourceSpan,
    #[help]
    pub help: String,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Total number of articles to fetch
    #[arg(short, long, default_value_t = 3)]
    num_articles: usize,
    /// Number of most recent articles to get from each feed
    #[arg(short, long, default_value_t = 1)]
    per_source: usize,
    /// File with URLs of Atom/RSS feeds to read (one URL per line, lines starting with '#' or "//" are ignored)
    #[arg(short = 'S', long, value_name = "FILE", value_hint=ValueHint::FilePath)]
    url_file: Option<PathBuf>,
    /// Tera template file
    #[arg(short, long, value_parser, value_name = "FILE", value_hint=ValueHint::FilePath)]
    template_file: PathBuf,
    /// A single URL to consider (can be repeated to specify multiple)
    #[arg(short = 's', long, value_hint=ValueHint::Url)]
    url: Vec<Url>,
    /// Only include articles before this date (in YYYY-MM-DD format).
    ///
    /// This is naive (no timezone), so articles close to the boundary in different timezones might
    /// be unexpectedly filtered. In addition, some feeds are truncated, and may have already pruned
    /// away articles before this date from the feed itself.
    #[arg(short, long)]
    before: Option<Date>,
    /// Use request cache stored on disk at `.openringcache`
    ///
    /// Note that this only prevents refetching if the feed source responds
    /// with a 429. In this case, we respect Retry-After, or default to 4h.
    /// Otherwise, the existence of a cache file just allows openring to respect
    /// ETag and Last-Modified headers for conditional requests.
    #[arg(short, long)]
    cache: bool,
    /// Discard all cached requests older than this duration
    #[arg(
        long,
        value_parser = humantime::parse_duration,
        default_value = "14d"
    )]
    max_cache_age: Duration,
    #[clap(flatten)]
    pub verbose: Verbosity,
}

/// Describes a feed fetch result that can be serialized to disk
#[derive(Serialize, Deserialize, Debug)]
struct CacheValue {
    timestamp: Timestamp,
    retry_after: Option<Span>,
    last_modified: Option<String>,
    etag: Option<String>,
    body: Option<String>,
}

type Cache = DashMap<Url, CacheValue>;

trait StoreExt {
    /// Store the cache under the given path. Update access timestamps
    fn store<T: AsRef<Path>>(&self, path: T) -> Result<()>;

    /// Load cache from path. Discard entries older than `max_age_secs`
    fn load<T: AsRef<Path>>(path: T, max_age_secs: u64) -> Result<Cache>;
}

impl StoreExt for Cache {
    fn store<T: AsRef<Path>>(&self, path: T) -> Result<()> {
        let mut wtr = csv::WriterBuilder::new()
            .has_headers(false)
            .from_path(path)?;
        for result in self {
            wtr.serialize((result.key(), result.value()))?;
        }
        Ok(())
    }

    fn load<T: AsRef<Path>>(path: T, max_age_secs: u64) -> Result<Cache> {
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_path(path)?;

        let map = DashMap::new();
        let current_ts = Timestamp::now();
        for result in rdr.deserialize() {
            let (url, value): (Url, CacheValue) = result?;
            // Discard entries older than `max_age_secs`.
            // This allows gradually updating the cache over multiple runs.
            if (current_ts - value.timestamp).compare(i64::try_from(max_age_secs)?.seconds())?
                == Ordering::Less
            {
                map.insert(url, value);
            }
        }
        Ok(map)
    }
}

#[derive(Serialize, Debug)]
pub struct Article {
    link: Url,
    title: String,
    summary: String,
    source_link: Url,
    source_title: String,
    timestamp: Timestamp,
}

/// Load cache (if exists and is still valid).
/// This returns an `Option` as starting without a cache is a common scenario
/// and we silently discard errors on purpose.
fn load_cache(args: &Args) -> Option<Cache> {
    if !args.cache {
        return None;
    }

    // Discard entire cache if it hasn't been updated since `max_cache_age`.
    // This is an optimization, which avoids iterating over the file and
    // checking the age of each entry.
    match fs::metadata(OPENRING_CACHE_FILE) {
        Err(_e) => {
            // No cache found; silently start with empty cache
            return None;
        }
        Ok(metadata) => {
            let modified = metadata.modified().ok()?;
            let elapsed = modified.elapsed().ok()?;
            if elapsed > args.max_cache_age {
                warn!(
                    "Cache is too old (age: {:#?}, max age: {:#?}). Discarding and recreating.",
                    Duration::from_secs(elapsed.as_secs()),
                    Duration::from_secs(args.max_cache_age.as_secs())
                );
                return None;
            }
            info!(
                "Cache is recent (age: {:#?}, max age: {:#?}). Using.",
                Duration::from_secs(elapsed.as_secs()),
                Duration::from_secs(args.max_cache_age.as_secs())
            );
        }
    }

    let cache = Cache::load(OPENRING_CACHE_FILE, args.max_cache_age.as_secs());
    match cache {
        Ok(cache) => Some(cache),
        Err(e) => {
            warn!("Error while loading cache: {e}. Continuing without.");
            None
        }
    }
}

fn parse_urls_from_file(path: PathBuf) -> Result<Vec<Url>> {
    let file = File::open(path.clone())?;
    let reader = BufReader::new(file);

    reader
        .lines()
        // Allow '#' or "//" comments in the urls file
        .filter(|l| {
            let line = l.as_ref().unwrap();
            let trimmed = line.trim();
            !(trimmed.starts_with('#') || trimmed.starts_with("//"))
        })
        .map(|line| {
            let line = &line.unwrap();
            Url::parse(line).map_err(|e| {
                // Give a nice diagnostic error
                let file_src = fs::read_to_string(path.clone()).unwrap();
                let offset = file_src.find(line).unwrap();
                FeedUrlError {
                    src: NamedSource::new(
                        path.clone().into_os_string().to_string_lossy(),
                        file_src,
                    ),
                    span: (offset..offset + line.len()).into(),
                    help: e.to_string(),
                }
                .into()
            })
        })
        .collect()
}

// Get all feeds from URLs concurrently.
//
// Skips feeds if there are errors. Shows progress.
fn get_feeds_from_urls(urls: Vec<Url>, cache: &Arc<Cache>) -> Result<Vec<(Feed, Url)>> {
    let agent: Agent = AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!(crate_name!(), '/', crate_version!()))
        .build();

    let m = MultiProgress::new();

    let feeds: Vec<(Feed, Url)> = urls
        .par_iter()
        .enumerate()
        .filter_map(|(idx, url)| 'a: {
            let pb = m.add(ProgressBar::new(2));
            pb.set_style(ProgressStyle::with_template("{prefix:.bold.dim} {wide_msg}").unwrap());
            pb.set_prefix(format!("[{}/{}]", idx, urls.len()));
            pb.set_message(url.as_str().to_string());

            let cache_value = cache.get_mut(url);

            // Respect Retry-After Header
            if let Some(ref cv) = cache_value {
                if let Some(retry) = cv.retry_after {
                    if cv.timestamp + retry > Timestamp::now() {
                        debug!(timestamp=%cv.timestamp, retry_after=%retry, "skipping request due to 429, using feed from cache");

                        let body = cv.body.clone();

                        // TODO: This is just copy-pasted, should be reused
                        pb.inc(1);
                        if let Some(feed_str) = body {
                            match parser::parse(feed_str.as_bytes()) {
                                Ok(feed) => {
                                    pb.finish_and_clear();
                                    break 'a Some((feed, url.clone()));
                                }
                                Err(e) => {
                                    warn!(
                                        url=%url.as_str(),
                                        error=%e,
                                        "failed to parse feed."
                                    );
                                    pb.finish_with_message(format!(
                                        "Failed to parse feed from `{}`",
                                        url.as_str()
                                    ));
                                    break 'a None;
                                }
                            }
                        } else {
                            warn!(url=url.as_str(), "empty feed");
                            pb.finish_with_message(format!("Empty feed: `{}`", url.as_str()));
                            break 'a None;
                        }
                    }
                }
            }

            let req = {
                let mut r = agent.get(url.as_str());
                // Add friendly headers if cache is available
                if let Some(ref cv) = cache_value {
                    if let Some(last_modified) = &cv.last_modified {
                        r = r.set("If-Modified-Since", last_modified);
                    }
                    if let Some(etag) = &cv.etag {
                        r = r.set("If-None-Match", etag);
                    }
                }
                debug!(url=%url, if_modified_since=r.header("if-modified-since"), etag=r.header("if-none-match"), "sending request");
                r
            };

            let body = match req.call() {
                Ok(r) => {
                    // ETag values must have the actual quotes
                    let etag = r.header("etag").map(|s| {
                        if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with("W/\"") && s.ends_with('"')) {
                            s.to_string()
                        } else {
                            format!("\"{}\"", s)
                        }}
                    );
                    let last_modified = r.header("last-modified").map(|s| s.to_string());
                    let status = r.status();
                    let mut body = r.into_string().ok();

                    // Update cache
                    if let Some(mut cv) = cache_value {
                        match status {
                            304 => {
                                debug!(url=%url, status=status, "got 304, using feed from cache");
                                body = cv.body.clone();
                            },
                            _ =>  {
                                debug!(url=%url, status=status, "cache hit, using feed from body");
                                cv.etag = etag;
                                cv.last_modified = last_modified;
                                cv.body = body.clone();
                            }
                        }
                        cv.timestamp = Timestamp::now();
                    } else {
                        debug!(url=%url, status=status, "using feed from body and adding to cache");
                        cache.insert(
                            url.clone(),
                            CacheValue {
                                timestamp: Timestamp::now(),
                                retry_after: None,
                                etag,
                                last_modified,
                                body: body.clone(),
                            },
                        );
                    }
                    body
                }
                Err(e) => {
                    if let Some(mut cv) = cache_value {
                        cv.timestamp = Timestamp::now();
                        match e {
                            ureq::Error::Status(status, r) if status == 429 => {
                                // Default to waiting 4 hrs if no Retry-After
                                let retry_after= r.header("retry-after").map(|s| s.parse::<i64>().map(|n| n.seconds()).ok()).unwrap_or(Some(4.hours()));
                                debug!(url=%url, status=status, retry_after=r.header("retry-after"), "got 429, using feed from cache");
                                dbg!(&retry_after);
                                cv.timestamp = Timestamp::now();
                                cv.retry_after = retry_after;
                                cv.body.clone()
                            },
                            _ => None
                        }
                    } else {
                        warn!(url=%url.as_str(), error=%e, "failed to get feed.");
                        None
                    }
                }
            };
            pb.inc(1);

            if let Some(feed_str) = body {
                match parser::parse(feed_str.as_bytes()) {
                    Ok(feed) => {
                        pb.finish_and_clear();
                        Some((feed, url.clone()))
                    }
                    Err(e) => {
                        warn!(
                            url=%url.as_str(),
                            error=%e,
                            "failed to parse feed."
                        );
                        pb.finish_with_message(format!(
                            "Failed to parse feed from `{}`",
                            url.as_str()
                        ));
                        None
                    }
                }
            } else {
                warn!(url=url.as_str(), "empty feed");
                pb.finish_with_message(format!("Empty feed: `{}`", url.as_str()));
                None
            }
        })
        .collect();
    m.clear()?;
    Ok(feeds)
}

pub fn run(args: Args) -> Result<()> {
    debug!(?args);
    let cache = load_cache(&args).unwrap_or_default();
    let cache = Arc::new(cache);

    let mut urls = args.url;

    if let Some(path) = args.url_file {
        let mut file_urls = parse_urls_from_file(path)?;
        urls.append(&mut file_urls);
    };

    if urls.is_empty() {
        return Err(OpenringError::FeedMissing);
    }

    let feeds = get_feeds_from_urls(urls, &cache)?;

    if args.cache {
        cache.store(OPENRING_CACHE_FILE)?;
    }

    let template = fs::read_to_string(&args.template_file)?;
    let mut context = tera::Context::new();

    // Grab articles from all the feeds
    let mut articles = Vec::new();
    for (feed, url) in feeds {
        let entries = if feed.entries.len() >= args.per_source {
            &feed.entries[0..args.per_source]
        } else {
            &feed.entries
        };

        let source_title = match feed.title {
            Some(ref t) => {
                if t.content.is_empty() {
                    url.domain().unwrap().to_owned()
                } else {
                    t.content.clone()
                }
            }
            None => url.domain().unwrap().to_owned(),
        };
        let source_link = match &feed.title.as_ref().unwrap().src {
            None => {
                // Then, look for links
                match feed
                    .links
                    .iter()
                    .find(|l| {
                        if let Some(rel) = &l.rel {
                            rel == "alternate"
                        } else {
                            false
                        }
                    })
                    .map(|l| &l.href)
                {
                    None => {
                        // If an alternate link is missing just grab one of them
                        match feed
                            .links
                            .into_iter()
                            // Ignore "self" rels, which usually link to feed
                            .find(|l| !l.rel.as_ref().is_some_and(|r| r == "self"))
                            .map(|l| l.href)
                        {
                            Some(s) => {
                                match Url::parse(&s) {
                                    Ok(u) => u,
                                    Err(ParseError::RelativeUrlWithoutBase) => Url::parse(
                                        &format!("{}{}", url.origin().ascii_serialization(), &s),
                                    )?,
                                    Err(e) => return Err(OpenringError::UrlParseError(e)),
                                }
                            }
                            None => return Err(OpenringError::FeedBadTitle(url.to_string())),
                        }
                    }
                    Some(s) => match Url::parse(s) {
                        Ok(u) => u,
                        Err(ParseError::RelativeUrlWithoutBase) => {
                            Url::parse(&format!("{}{}", url.origin().ascii_serialization(), &s))?
                        }
                        Err(e) => return Err(OpenringError::UrlParseError(e)),
                    },
                }
            }
            Some(s) => match Url::parse(s) {
                Ok(u) => u,
                Err(ParseError::RelativeUrlWithoutBase) => {
                    Url::parse(&format!("{}{}", url.origin().ascii_serialization(), &s))?
                }
                Err(e) => return Err(OpenringError::UrlParseError(e)),
            },
        };
        for entry in entries.iter() {
            if let (Some(link), Some(title), Some(date)) =
                (
                    match entry
                        .links
                        .iter()
                        .find(|l| {
                            if let Some(rel) = &l.rel {
                                rel == "alternate"
                            } else {
                                false
                            }
                        })
                        .map(|l| &l.href)
                    {
                        Some(s) => match Url::parse(s) {
                            Ok(u) => Some(u),
                            Err(ParseError::RelativeUrlWithoutBase) => {
                                Url::parse(&format!("{}{}", url.origin().ascii_serialization(), &s))
                                    .ok()
                            }
                            Err(_) => None,
                        },
                        None => {
                            // If an alternate link is missing just grab one of them
                            match entry.links.clone().into_iter().next().map(|l| l.href) {
                                Some(s) => match Url::parse(&s) {
                                    Ok(u) => Some(u),
                                    Err(ParseError::RelativeUrlWithoutBase) => Url::parse(
                                        &format!("{}{}", url.origin().ascii_serialization(), &s),
                                    )
                                    .ok(),
                                    Err(_) => None,
                                },
                                None => return Err(OpenringError::FeedBadTitle(url.to_string())),
                            }
                        }
                    },
                    entry.title.as_ref().map(|t| &t.content),
                    entry.published.or(entry.updated),
                )
            {
                // Skip articles after args.before, if present
                let timestamp = Timestamp::from_second(date.timestamp())?;
                if let Some(before) = args.before {
                    if timestamp > before.to_zoned(TimeZone::system())?.timestamp() {
                        continue;
                    }
                }

                let summary = match &entry.summary {
                    Some(s) => &s.content,
                    None => match &entry.content {
                        Some(c) => match &c.body {
                            Some(b) => b,
                            None => {
                                info!(?link, ?source_link, "no summary or content provided.");
                                ""
                            }
                        },
                        None => {
                            info!(?link, ?source_link, "no summary or content provided.");
                            ""
                        }
                    },
                };

                let mut safe_summary = String::new();
                html_escape::decode_html_entities_to_string(
                    ammonia::clean(summary),
                    &mut safe_summary,
                );
                articles.push(Article {
                    link,
                    title: title.to_string(),
                    summary: safe_summary.trim().to_string(),
                    source_link: source_link.clone(),
                    source_title: source_title.clone(),
                    timestamp,
                });
            } else {
                warn!(
                    entry_links=?entry.links,
                    entry_title=?entry.title,
                    entry_published=?entry.published,
                    entry_updated=?entry.updated,
                    source=url.as_str(),
                    "skipping entry: must have link, title, and a date."
                );
            }
        }
    }

    articles.sort_unstable_by(|a, b| a.timestamp.cmp(&b.timestamp).reverse());
    let articles = if articles.len() >= args.num_articles {
        &articles[0..args.num_articles]
    } else {
        &articles
    };

    context.insert("articles", articles);
    // TODO: this validation of the template should come before all the time spent fetching feeds.
    let output = Tera::one_off(&template, &context, true)?;
    println!("{output}");
    Ok(())
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
