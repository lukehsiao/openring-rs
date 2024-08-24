pub mod args;
pub mod cache;
pub mod error;

use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use clap::{crate_name, crate_version};
use feed_rs::{model::Feed, parser};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use jiff::{tz::TimeZone, Timestamp, ToSpan};
use miette::NamedSource;
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use serde::Serialize;
use tera::Tera;
use tracing::{debug, info, warn};
use ureq::{Agent, AgentBuilder};
use url::{ParseError, Url};

use crate::{
    args::Args,
    cache::{Cache, CacheValue, StoreExt, OPENRING_CACHE_FILE},
    error::{FeedUrlError, OpenringError, Result},
};

#[derive(Serialize, Debug)]
pub struct Article {
    link: Url,
    title: String,
    summary: String,
    source_link: Url,
    source_title: String,
    timestamp: Timestamp,
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
    let cache = cache::load_cache(&args).unwrap_or_default();
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
