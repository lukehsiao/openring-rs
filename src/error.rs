#![expect(clippy::module_name_repetitions)]
use std::result;

use miette::{Diagnostic, NamedSource, SourceSpan};
use thiserror::Error;

pub(crate) type Result<T> = result::Result<T, OpenringError>;

#[derive(Error, Debug, Diagnostic)]
pub enum OpenringError {
    #[error("No feed urls were provided. Provide feeds with -s or -S <FILE>.")]
    FeedMissing,
    #[error("Failed to find when the `--before` date starts in the local time zone.")]
    CivilDateError(#[source] jiff::Error),
    #[error(transparent)]
    HttpError(#[from] crate::http::TransportError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    FeedUrlError(#[from] FeedUrlError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    FeedWeightError(#[from] FeedWeightError),
    #[error("Failed to read `{path}`.")]
    #[diagnostic(code(openring::read_file_error))]
    ReadFileError {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Failed to write the output.")]
    #[diagnostic(code(openring::output_error))]
    OutputError(#[source] std::io::Error),
    #[error("Failed to parse tera template.")]
    #[diagnostic(code(openring::template_error))]
    TemplateError(#[from] tera::Error),
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

#[derive(Error, Diagnostic, Debug)]
#[error("Invalid feed weight.")]
#[diagnostic(code(openring::feed_weight_error))]
pub struct FeedWeightError {
    #[source_code]
    pub src: NamedSource<String>,
    #[label("this weight is invalid")]
    pub span: SourceSpan,
    #[help]
    pub help: String,
}
