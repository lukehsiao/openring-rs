use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    path::PathBuf,
    result,
    time::Duration,
};

use chrono::{
    naive::{NaiveDate, NaiveDateTime},
    DateTime, FixedOffset, Local, TimeZone,
};
use clap::{builder::ValueHint, crate_name, crate_version, Parser};
use clap_verbosity_flag::{Verbosity, WarnLevel};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use miette::{Diagnostic, NamedSource, SourceSpan};
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use serde::Serialize;
use syndication::Feed;
use tera::Tera;
use thiserror::Error;
use tracing::{debug, info, warn};
use ureq::{Agent, AgentBuilder};
use url::Url;

type Result<T> = result::Result<T, OpenringError>;

#[derive(Error, Debug, Diagnostic)]
pub enum OpenringError {
    #[error("No valid published or updated date found.")]
    DateError,
    #[error("No feed urls were provided. Provide feeds with -s or -S <FILE>.")]
    FeedMissing,
    #[error("Failed to parse naive date.")]
    NaiveDateError(#[from] chrono::ParseError),
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
}

#[derive(Error, Diagnostic, Debug)]
#[error("Failed to parse datetime.")]
#[diagnostic(code(openring::chrono_error))]
pub struct ChronoError {
    #[source_code]
    pub src: NamedSource,
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
    pub src: NamedSource,
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
    /// File with URLs of RSS feeds to read (one URL per line, lines starting with '#' or "//" ignored)
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
    #[arg(short, long, value_parser = parse_naive_date)]
    before: Option<NaiveDate>,
    #[clap(flatten)]
    pub verbose: Verbosity<WarnLevel>,
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
fn get_feeds_from_urls(urls: Vec<Url>) -> Result<Vec<(Feed, Url)>> {
    let agent: Agent = AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!(crate_name!(), '/', crate_version!()))
        .build();

    let m = MultiProgress::new();

    let feeds: Vec<(Feed, Url)> = urls
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
                    warn!(url=%url.as_str(), error=%e, "Failed to get feed");
                    None
                }
            };
            pb.inc(1);

            if let Some(feed_str) = body {
                match feed_str.parse::<Feed>() {
                    Ok(feed) => {
                        pb.finish_and_clear();
                        Some((feed, url.clone()))
                    }
                    Err(e) => {
                        warn!(
                            url=%url.as_str(),
                            error=%e,
                            "Failed to parse RSS/Atom feed."
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
    Ok(feeds)
}

// Parse the date, falling back to naive parsing if necessary.
fn parse_date(date: &str) -> Result<DateTime<FixedOffset>> {
    date.parse::<DateTime<FixedOffset>>()
        .or_else(|_| DateTime::parse_from_rfc2822(date))
        .or_else(|_| DateTime::parse_from_rfc3339(date))
        .or_else(|_| {
            debug!(?date, "attempting to parse non-standard date");
            let naive_dt = NaiveDateTime::parse_from_str(date, "%Y-%m-%dT%H:%M:%S")?;
            let fixed_offset =
                FixedOffset::east_opt(Local::now().offset().local_minus_utc()).unwrap();
            fixed_offset
                .from_local_datetime(&naive_dt)
                .earliest()
                .ok_or(OpenringError::DateError)
        })
}

pub fn run(args: Args) -> Result<()> {
    debug!(?args);
    let mut urls = args.url;

    if let Some(path) = args.url_file {
        let mut file_urls = parse_urls_from_file(path)?;
        urls.append(&mut file_urls);
    };

    if urls.is_empty() {
        return Err(OpenringError::FeedMissing);
    }

    let feeds = get_feeds_from_urls(urls)?;

    let template = fs::read_to_string(&args.template_file)?;
    let mut context = tera::Context::new();

    let mut articles = Vec::new();
    for (feed, url) in feeds {
        match feed {
            Feed::RSS(c) => {
                let items = if c.items().len() >= args.per_source {
                    &c.items()[0..args.per_source]
                } else {
                    c.items()
                };
                let source_link = Url::parse(c.link())?;
                let source_title = c.title().to_string();
                for item in items {
                    if let (Some(link), Some(title), Some(date)) =
                        (item.link(), item.title(), item.pub_date())
                    {
                        let date = parse_date(date)?;
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
                                    warn!(?link, ?source_link, "Skipping link from feed: no summary or content provided in feed.");
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
                            link: Url::parse(link)?,
                            title: title.to_string(),
                            summary: safe_summary.trim().to_string(),
                            source_link: source_link.clone(),
                            source_title: source_title.clone(),
                            date,
                        });
                    } else {
                        debug!(?item, "Skipping. Must have link, title, and date");
                    }
                }
            }
            Feed::Atom(ref f) => {
                let items = &f.entries()[0..args.per_source];
                let feed_links = f.links();
                if !feed_links.is_empty() {
                    debug!(?feed_links);

                    let source_link = Url::parse(
                        f.links()
                            .iter()
                            .find(|l| l.rel() == "alternate")
                            .unwrap()
                            .href(),
                    )?;
                    let source_title = f.title().to_string();
                    for item in items {
                        if !item.links().is_empty() {
                            let date = parse_date(item.updated()).or_else(|_| {
                                debug!("using published date, rather than last updated date");
                                if let Some(date) = item.published() {
                                    parse_date(date).map_err(|e| {
                                        let feed_src = feed.to_string();
                                        let start = feed_src.find(date).unwrap();
                                        let len = date.len();
                                        ChronoError {
                                            src: NamedSource::new(url.as_str(), feed_src),
                                            span: (start..start + len).into(),
                                            help: e.to_string(),
                                        }
                                        .into()
                                    })
                                } else {
                                    Err(OpenringError::DateError)
                                }
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
                                        warn!(link=%item.links()[0].href(), ?source_link, "Skipping link from feed: no summary or content provided in feed.");
                                        continue;
                                    }
                                },
                            };

                            let mut safe_summary = String::new();
                            html_escape::decode_html_entities_to_string(
                                ammonia::clean(&summary),
                                &mut safe_summary,
                            );
                            info!(summary = %summary);
                            // Uses the last link, since blogspot puts the article link last.
                            let link = Url::parse(
                                item.links()
                                    .iter()
                                    .find(|l| l.rel() == "alternate")
                                    .unwrap()
                                    .href(),
                            )?;
                            articles.push(Article {
                                link,
                                title: item.title().to_string(),
                                summary: safe_summary.trim().to_string(),
                                source_link: source_link.clone(),
                                source_title: source_title.clone(),
                                date,
                            });
                        } else {
                            debug!(?item, "Skipping. Must have links.");
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
