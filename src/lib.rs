use std::fs::{self, File};

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{naive::NaiveDate, DateTime, FixedOffset};
use clap::{builder::ValueHint, crate_name, crate_version, Parser};
use clap_verbosity_flag::Verbosity;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use serde::Serialize;
use syndication::Feed;
use tera::Tera;
use thiserror::Error;
use tracing::{debug, info, trace, warn};
use ureq::{Agent, AgentBuilder};
use url::Url;

#[derive(Error, Debug)]
pub enum OpenringError {
    #[error("No valid published or updated date found.")]
    DateError,
    #[error("No feed urls were provided. Provide feeds with -s or -S <FILE>.")]
    FeedMissing,
    #[error(transparent)]
    ChronoError(#[from] chrono::ParseError),
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
    /// File with URLs of RSS feeds to read (one URL per line)
    #[arg(short = 'S', long, value_name = "FILE", value_hint=ValueHint::FilePath)]
    url_file: Option<PathBuf>,
    /// Tera template file
    #[arg(short, long, value_parser, value_name = "FILE", value_hint=ValueHint::FilePath)]
    template_file: PathBuf,
    /// A specific URL to consider (can be repeated)
    #[arg(short = 's', long, value_hint=ValueHint::Url)]
    urls: Vec<Url>,
    /// Only include articles before this date (in YYYY-MM-DD format).
    ///
    /// This is naive (no timezone), so articles close to the boundary in different timezones might
    /// be unexpectedly filtered. In addition, some feeds are truncated, and may have already pruned
    /// away articles before this date from the feed itself.
    #[arg(short, long, value_parser = parse_naive_date)]
    before: Option<NaiveDate>,
    #[clap(flatten)]
    pub verbose: Verbosity,
}

#[derive(Serialize, Debug)]
pub struct Article {
    link: Url,
    title: String,
    summary: String,
    source_link: Url,
    source_title: String,
    date: DateTime<FixedOffset>,
}

fn parse_naive_date(input: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(input, "%Y-%m-%d").map_err(|e| e.into())
}

pub fn run(args: Args) -> Result<()> {
    trace!("Args: {:#?}", args);
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
    debug!(
        "Fetching these urls: {:#?}",
        urls.iter().map(|url| url.as_str()).collect::<Vec<&str>>()
    );

    if urls.is_empty() {
        bail!(OpenringError::FeedMissing)
    }

    let template = fs::read_to_string(&args.template_file)
        .with_context(|| format!("Failed to read file `{:?}`", args.template_file))?;
    let mut context = tera::Context::new();

    info!("Fetching feeds...");

    let agent: Agent = AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!(crate_name!(), '/', crate_version!()))
        .build();

    let m = MultiProgress::new();

    let feeds: Vec<Feed> = urls
        .par_iter()
        .enumerate()
        .filter_map(|(idx, url)| {
            let pb = m.add(ProgressBar::new(2));
            pb.set_style(ProgressStyle::with_template("{prefix:.bold.dim} {wide_msg}").unwrap());
            pb.set_prefix(format!("[{}/{}]", idx, urls.len()));
            pb.set_message(url.as_str().to_string());
            let body = match agent.get(url.as_str()).call() {
                Ok(r) => r.into_string().ok(),
                Err(e) => {
                    warn!("Unable to get feed `{}`\n\nCaused By:\n{}", url.as_str(), e);
                    None
                }
            };
            pb.inc(1);

            if let Some(feed_str) = body {
                match feed_str.parse::<Feed>() {
                    Ok(feed) => {
                        pb.finish_and_clear();
                        Some(feed)
                    }
                    Err(e) => {
                        warn!(
                            "Failed to parse RSS/Atom feed from `{}`\n\nCaused By:\n{}",
                            url.as_str(),
                            e
                        );
                        pb.finish_with_message(format!(
                            "Failed to parse feed from `{}`",
                            url.as_str()
                        ));
                        None
                    }
                }
            } else {
                pb.finish_with_message(format!("Empty feed: `{}`", url.as_str()));
                None
            }
        })
        .collect();
    m.clear()?;

    let mut articles = Vec::new();
    for feed in feeds {
        match feed {
            Feed::RSS(c) => {
                let items = if c.items().len() >= args.per_source {
                    &c.items()[0..args.per_source]
                } else {
                    c.items()
                };
                let source_link = Url::parse(c.link())
                    .with_context(|| format!("Unabled to parse url `{}`", c.link()))?;
                let source_title = c.title().to_string();
                for item in items {
                    if let (Some(link), Some(title), Some(date)) =
                        (item.link(), item.title(), item.pub_date())
                    {
                        let date = date
                            .parse::<DateTime<FixedOffset>>()
                            .or_else(|_| DateTime::parse_from_rfc2822(date))
                            .with_context(|| format!("Unabled to parse date `{}`", date))?;

                        // Skip articles after args.before, if present
                        if let Some(before) = args.before {
                            if date.date_naive() > before {
                                continue;
                            }
                        }

                        let summary = match item.description() {
                            Some(s) => s.to_string(),
                            None => match item.content() {
                                Some(s) => s.to_string(),
                                None => {
                                    warn!("Skipping `{}` from `{}`, no summary or content provided in feed.", link, source_link);
                                    continue;
                                }
                            },
                        };
                        let mut safe_summary = String::new();
                        html_escape::decode_html_entities_to_string(
                            ammonia::clean(&summary),
                            &mut safe_summary,
                        );
                        articles.push(Article {
                            link: Url::parse(link)
                                .with_context(|| format!("Unabled to parse url `{}`", c.link()))?,
                            title: title.to_string(),
                            summary: safe_summary.trim().to_string(),
                            source_link: source_link.clone(),
                            source_title: source_title.clone(),
                            date,
                        });
                    } else {
                        debug!("Skipping `{:#?}`, must have link, title, and date", item);
                    }
                }
            }
            Feed::Atom(f) => {
                let items = &f.entries()[0..args.per_source];
                let feed_links = f.links();
                if !feed_links.is_empty() {
                    trace!("Feed links: {:#?}", feed_links);

                    let source_link = Url::parse(
                        f.links()
                            .iter()
                            .find(|l| l.rel() == "alternate")
                            .unwrap()
                            .href(),
                    )
                    .with_context(|| format!("Unabled to parse url `{}`", f.links()[0].href()))?;
                    let source_title = f.title().to_string();
                    for item in items {
                        if !item.links().is_empty() {
                            let date = item
                                .updated()
                                .parse::<DateTime<FixedOffset>>()
                                .or_else(|_| DateTime::parse_from_rfc2822(item.updated()))
                                .or_else(|_| {
                                    debug!("Using published date, rather than last updated date.");
                                    if let Some(date) = item.published() {
                                        date.parse::<DateTime<FixedOffset>>()
                                            .or_else(|_| DateTime::parse_from_rfc2822(date))
                                            .map_err(OpenringError::ChronoError)
                                    } else {
                                        Err(OpenringError::DateError)
                                    }
                                })
                                .with_context(|| {
                                    format!("Unabled to parse date `{}`", item.updated())
                                })?;

                            // Skip articles after args.before, if present
                            if let Some(before) = args.before {
                                if date.date_naive() > before {
                                    continue;
                                }
                            }

                            let summary = match item.summary() {
                                Some(s) => s.to_string(),
                                None => match item.content().map(|c| c.value()) {
                                    Some(Some(v)) => v.to_string(),
                                    _ => {
                                        warn!("Skipping `{}` from `{}`, no summary or content provided in feed.", item.links()[0].href(), source_link);
                                        continue;
                                    }
                                },
                            };

                            let mut safe_summary = String::new();
                            html_escape::decode_html_entities_to_string(
                                ammonia::clean(&summary),
                                &mut safe_summary,
                            );
                            warn!("{:#?}", safe_summary);
                            // Uses the last link, since blogspot puts the article link last.
                            let link = Url::parse(
                                item.links()
                                    .iter()
                                    .find(|l| l.rel() == "alternate")
                                    .unwrap()
                                    .href(),
                            )
                            .with_context(|| {
                                format!("Unabled to parse url `{}`", f.links()[0].href())
                            })?;
                            articles.push(Article {
                                link,
                                title: item.title().to_string(),
                                summary: safe_summary.trim().to_string(),
                                source_link: source_link.clone(),
                                source_title: source_title.clone(),
                                date,
                            });
                        } else {
                            debug!("Skipping `{:#?}`, must have links", item);
                        }
                    }
                }
            }
        }
    }

    articles.sort_unstable_by(|a, b| a.date.cmp(&b.date).reverse());
    let articles = if articles.len() >= args.num_articles {
        &articles[0..args.num_articles]
    } else {
        &articles
    };

    context.insert("articles", articles);
    // TODO: this validation of the template should come before all the time spent fetching feeds.
    let output = Tera::one_off(&template, &context, true)
        .with_context(|| format!("Failed to parse Tera template:\n{}", template))?;
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
