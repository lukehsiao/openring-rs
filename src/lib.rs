use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    path::PathBuf,
    result,
    time::Duration,
};

use chrono::{naive::NaiveDate, DateTime, Utc};
use clap::{builder::ValueHint, crate_name, crate_version, Parser};
use clap_verbosity_flag::Verbosity;
use feed_rs::{model::Feed, parser};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use miette::{Diagnostic, NamedSource, SourceSpan};
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use serde::Serialize;
use tera::Tera;
use thiserror::Error;
use tracing::{debug, warn};
use ureq::{Agent, AgentBuilder};
use url::{ParseError, Url};

type Result<T> = result::Result<T, OpenringError>;

#[derive(Error, Debug, Diagnostic)]
pub enum OpenringError {
    #[error("No valid published or updated date found.")]
    DateError,
    #[error("No feed urls were provided. Provide feeds with -s or -S <FILE>.")]
    FeedMissing,
    #[error("The feed at `{0}` has a bad a title (e.g., missing link or title).")]
    #[diagnostic(code(openring::feed_title_error))]
    FeedBadTitle(String),
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
    pub verbose: Verbosity,
}

#[derive(Serialize, Debug)]
pub struct Article {
    link: Url,
    title: String,
    summary: String,
    source_link: Url,
    source_title: String,
    date: DateTime<Utc>,
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
                    warn!(url=%url.as_str(), error=%e, "failed to get feed.");
                    None
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
                if let Some(before) = args.before {
                    if date.date_naive() > before {
                        continue;
                    }
                }

                let summary = match &entry.summary {
                    Some(s) => &s.content,
                    None => match &entry.content {
                        Some(c) => match &c.body {
                            Some(b) => b,
                            None => {
                                warn!(
                                    ?link,
                                    ?source_link,
                                    "skipping link from feed: no summary or content provided."
                                );
                                continue;
                            }
                        },
                        None => {
                            warn!(
                                ?link,
                                ?source_link,
                                "skipping link from feed: no summary or content provided."
                            );
                            continue;
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
                    date,
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
