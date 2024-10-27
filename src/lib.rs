pub mod args;
pub mod cache;
pub mod error;
pub mod feedfetcher;

use std::{
    collections::HashSet,
    fs::{self, File},
    io::{BufRead, BufReader},
    path::Path,
    sync::Arc,
};

use feed_rs::model::Feed;
use indicatif::{ProgressBar, ProgressStyle};
use jiff::{tz::TimeZone, Timestamp};
use miette::NamedSource;
use serde::Serialize;
use tera::Tera;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};
use url::{ParseError, Url};
use yansi::Paint;

use crate::{
    args::Args,
    cache::{Cache, StoreExt, OPENRING_CACHE_FILE},
    error::{FeedUrlError, OpenringError, Result},
    feedfetcher::FeedFetcher,
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

/// Parse the file into a vector of URLs.
fn parse_urls_from_file(path: &Path) -> Result<Vec<Url>> {
    let file = File::open(path)?;
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
                let file_src = fs::read_to_string(path).unwrap();
                let offset = file_src.find(line).unwrap();
                FeedUrlError {
                    src: NamedSource::new(
                        path.to_path_buf().into_os_string().to_string_lossy(),
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
async fn get_feeds_from_urls(urls: &[Url], cache: &Arc<Cache>) -> Vec<(Feed, Url)> {
    let pb = ProgressBar::new(urls.len() as u64).with_style(
        ProgressStyle::with_template("{prefix:>8} [{bar}] {human_pos}/{human_len}: {wide_msg}")
            .unwrap(),
    );
    pb.set_prefix("Fetching".bold().to_string());

    let mut join_set = JoinSet::new();
    let mut pending_urls: HashSet<&Url> = HashSet::from_iter(urls);

    pb.set_message(
        pending_urls
            .iter()
            .map(|u| u.as_str())
            .collect::<Vec<&str>>()
            .join(", "),
    );

    for url in urls {
        let cache_clone = Arc::clone(cache);
        let url_clone = url.clone();
        join_set.spawn(async move {
            let fetch_result = url_clone.fetch_feed(&cache_clone).await;
            (url_clone, fetch_result)
        });
    }
    let mut feeds = Vec::new();

    while let Some(result) = join_set.join_next().await {
        pb.inc(1);
        match result {
            Ok((url, Ok(feed))) => {
                pending_urls.remove(&url);
                pb.set_message(
                    pending_urls
                        .iter()
                        .map(|u| u.as_str())
                        .collect::<Vec<&str>>()
                        .join(", "),
                );
                pb.println(format!("{:>8} {url}", "Fetched".bold().green()));
                feeds.push((feed, url));
            }
            Ok((url, Err(e))) => {
                pending_urls.remove(&url);
                pb.set_message(
                    pending_urls
                        .iter()
                        .map(|u| u.as_str())
                        .collect::<Vec<&str>>()
                        .join(", "),
                );
                pb.println(format!("{:>8} {url} ({e})", "Error".bold().red()));
            }
            _ => (),
        }
    }

    pb.finish_and_clear();
    feeds
}

#[allow(clippy::missing_panics_doc)]
#[allow(clippy::missing_errors_doc)]
#[allow(clippy::too_many_lines)]
pub async fn run(args: Args) -> Result<()> {
    debug!(?args);
    let cache = cache::load_cache(&args).unwrap_or_default();
    let cache = Arc::new(cache);

    let mut urls = args.url;

    if let Some(path) = args.url_file {
        let mut file_urls = parse_urls_from_file(&path)?;
        urls.append(&mut file_urls);
    };

    if urls.is_empty() {
        return Err(OpenringError::FeedMissing);
    }

    // Deduplicate
    let urls: Vec<Url> = {
        let unique: HashSet<Url> = urls.into_iter().collect();
        unique.into_iter().collect()
    };

    let feeds = get_feeds_from_urls(&urls, &cache).await;

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
        for entry in entries {
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
                    None => {
                        if let Some(c) = &entry.content {
                            if let Some(b) = &c.body {
                                b
                            } else {
                                info!(?link, ?source_link, "no summary or content provided.");
                                ""
                            }
                        } else {
                            info!(?link, ?source_link, "no summary or content provided.");
                            ""
                        }
                    }
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
