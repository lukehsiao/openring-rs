#![expect(clippy::module_name_repetitions)]
use std::result;

use miette::Diagnostic;
use thiserror::Error;

pub(crate) type Result<T> = result::Result<T, OpenringError>;

#[derive(Error, Debug, Diagnostic)]
pub enum OpenringError {
    #[error("Failed to find when the `--before` date starts in the local time zone.")]
    CivilDateError(#[source] jiff::Error),
    #[error(transparent)]
    HttpError(#[from] crate::http::TransportError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    FeedList(#[from] crate::feed_list::Error),
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
    #[error("Failed to parse the template.")]
    #[diagnostic(code(openring::template_error))]
    TemplateError(#[source] tera::Error),
    #[error("Failed to render the template.")]
    #[diagnostic(code(openring::render_error))]
    RenderError(#[source] tera::Error),
}
