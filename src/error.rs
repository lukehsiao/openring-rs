#![allow(clippy::module_name_repetitions)]
#![allow(unused_assignments)]
use std::result;

use miette::{Diagnostic, NamedSource, SourceSpan};
use thiserror::Error;

pub(crate) type Result<T> = result::Result<T, OpenringError>;

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
    ReqwestError(#[from] reqwest::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    ChronoError(#[from] ChronoError),
    #[error(transparent)]
    #[diagnostic(code(openring::cache_error))]
    SerdeJsonError(#[from] serde_json::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    FeedUrlError(#[from] FeedUrlError),
    #[error("The feed at `{0}` was empty.")]
    #[diagnostic(code(openring::empty_feed_error))]
    EmptyFeedError(String),
    #[error("The request feed at `{0}` was rate limited (HTTP 429).")]
    #[diagnostic(code(openring::rate_limit_error))]
    RateLimitError(String),
    #[error("The request feed at `{url}` received an unexpected error (HTTP {status}).")]
    #[diagnostic(code(openring::unexpected_status_error))]
    UnexpectedStatusError { url: String, status: String },
    #[error(transparent)]
    #[diagnostic(code(openring::parse_feed_error))]
    ParseFeedError(#[from] feed_rs::parser::ParseFeedError),
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
