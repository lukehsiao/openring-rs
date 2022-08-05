use std::fs::{self, File};

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{serde::ts_seconds, DateTime, Utc};
use clap::{builder::ValueHint, crate_name, crate_version, Parser};
use log::{debug, info, trace, warn};
use serde::Serialize;
use syndication::Feed;
use tera::Tera;
use ureq::{Agent, AgentBuilder};
use url::Url;

#[derive(Parser, Debug)]
#[clap(author, version, about)]
pub struct Args {
    /// Total number of articles to fetch
    #[clap(short, long, value_parser, default_value_t = 3)]
    num_articles: usize,
    /// Number of most recent articles to get from each feed
    #[clap(short, long, value_parser, default_value_t = 1)]
    per_source: usize,
    /// File with URLs of RSS feeds to read
    #[clap(short = 'S', long, value_parser, value_name = "FILE", value_hint=ValueHint::FilePath)]
    url_file: Option<PathBuf>,
    /// Tera template file
    #[clap(short, long, value_parser, value_name = "FILE", value_hint=ValueHint::FilePath)]
    template_file: PathBuf,
    /// A specific URL to consider (can be repeated)
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

    let agent: Agent = AgentBuilder::new()
        .timeout(Duration::from_secs(5))
        .user_agent(concat!(crate_name!(), '/', crate_version!()))
        .build();

    let feeds: Vec<Feed> = urls
        .iter()
        .filter_map(|url| {
            let body = match agent.get(url.as_str()).call() {
                Ok(r) => r.into_string().ok(),
                Err(e) => {
                    warn!("Unable to get feed `{}`\n\nCaused By:\n{}", url.as_str(), e);
                    None
                }
            };
            if let Some(feed_str) = body {
                match feed_str.parse::<Feed>() {
                    Ok(feed) => Some(feed),
                    Err(e) => {
                        warn!(
                            "Failed to parse RSS/Atom feed from `{}`\n\nCaused By:\n{}",
                            url.as_str(),
                            e
                        );
                        None
                    }
                }
            } else {
                None
            }
        })
        .collect();

    let mut articles = Vec::new();
    for feed in feeds {
        match feed {
            Feed::RSS(c) => {
                let items = &c.items()[0..args.per_source];
                let source_link = Url::parse(c.link())?;
                let source_title = c.title().to_string();
                for item in items {
                    if let (Some(link), Some(title), Some(date)) =
                        (item.link(), item.title(), item.pub_date())
                    {
                        let summary = match item.description() {
                            Some(s) => s.to_string(),
                            None => match item.content() {
                                Some(s) => s.to_string(),
                                None => {
                                    warn!("Skipping `{}`, no summary or content.", link);
                                    continue;
                                }
                            },
                        };
                        let safe_summary = ammonia::clean(&summary);
                        articles.push(Article {
                            link: Url::parse(link)?,
                            title: title.to_string(),
                            summary: safe_summary,
                            source_link: source_link.clone(),
                            source_title: source_title.clone(),
                            date: date.parse::<DateTime<Utc>>()?,
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

                    // TODO: just using the first for simplicity
                    let source_link = Url::parse(f.links()[0].href())?;
                    let source_title = f.title().to_string();
                    for item in items {
                        if !item.links().is_empty() {
                            let summary = match item.summary() {
                                Some(s) => s.to_string(),
                                None => match item.content().map(|c| c.value()) {
                                    Some(Some(v)) => v.to_string(),
                                    _ => {
                                        continue;
                                    }
                                },
                            };
                            let safe_summary = ammonia::clean(&summary);
                            let link = Url::parse(item.links()[0].href())?;
                            articles.push(Article {
                                link,
                                title: item.title().to_string(),
                                summary: safe_summary,
                                source_link: source_link.clone(),
                                source_title: source_title.clone(),
                                date: item.updated().parse::<DateTime<Utc>>()?,
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
    let articles = &articles[0..args.num_articles];

    context.insert("articles", articles);
    // TODO: this validation of the template should come before all the time spent fetching feeds.
    let output = Tera::one_off(&template, &context, true)
        .with_context(|| format!("Failed to parse Tera template:\n{}", template))?;
    println!("{output}");
    Ok(())
}
