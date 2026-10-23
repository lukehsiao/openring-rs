//! The feed list module: turns the `-s` values and the urls file into the
//! Feed list, one Feed per URL with at most one Weight. It owns the
//! `URL [WEIGHT]` grammar, the comment rules of the urls file, how repeated
//! listings merge, and every diagnostic for a bad listing, each pointing at
//! the offending token.
//!
//! It takes text, not paths: `run` reads the urls file.

use std::{collections::HashMap, num::NonZeroUsize, ops::Range, path::Path};

use miette::{NamedSource, SourceSpan};
use url::Url;

use crate::error::{FeedUrlError, FeedWeightError, OpenringError, Result};

/// A web feed the user configured, with its optional Weight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Feed {
    pub(crate) url: Url,
    pub(crate) weight: Option<NonZeroUsize>,
}

/// The urls file as `run` read it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UrlsFile<'a> {
    /// Where the file came from, for diagnostics.
    pub(crate) path: &'a Path,
    pub(crate) text: &'a str,
}

/// The Feed list from the `-s` values in `cli` and the urls `file`, in the
/// order each URL first appears: `-s` values first, then the file.
///
/// Each listing is `URL [WEIGHT]`. In the file, blank lines and lines that
/// start with `#` or `//` are skipped. Listing a URL again merges with the
/// first listing: a Weight wins over no Weight, and the same Weight twice
/// is fine.
///
/// # Errors
///
/// Returns [`OpenringError::FeedMissing`] when nothing is listed. Returns a
/// [`FeedUrlError`] or [`FeedWeightError`] for the first listing that does
/// not parse, or whose Weight differs from one the same URL already has.
/// Each points at the offending token in the `-s` value or the file.
pub(crate) fn parse(cli: &[String], file: Option<UrlsFile<'_>>) -> Result<Vec<Feed>> {
    let mut list = FeedList::default();
    for raw in cli {
        let origin = Origin::Cli(raw);
        list.add(parse_listing(raw, &origin, 0)?, &origin)?;
    }
    if let Some(file) = file {
        let origin = Origin::File(file);
        let mut offset = 0;
        for raw_line in file.text.split_inclusive('\n') {
            let line = raw_line.trim();
            if !(line.is_empty() || line.starts_with('#') || line.starts_with("//")) {
                list.add(parse_listing(raw_line, &origin, offset)?, &origin)?;
            }
            offset += raw_line.len();
        }
    }
    if list.feeds.is_empty() {
        return Err(OpenringError::FeedMissing);
    }
    Ok(list.feeds)
}

/// Where a listing came from.
enum Origin<'a> {
    Cli(&'a str),
    File(UrlsFile<'a>),
}

impl Origin<'_> {
    fn named(&self) -> NamedSource<String> {
        match self {
            Self::Cli(raw) => NamedSource::new("-s/--url", (*raw).to_owned()),
            Self::File(file) => NamedSource::new(file.path.to_string_lossy(), file.text.to_owned()),
        }
    }

    /// How a later conflict names this origin.
    fn describe(&self) -> &'static str {
        match self {
            Self::Cli(_) => "by -s/--url",
            Self::File(_) => "earlier in the urls file",
        }
    }
}

/// The Feeds so far, and where each recorded Weight came from.
#[derive(Default)]
struct FeedList {
    feeds: Vec<Feed>,
    /// Each URL's index in `feeds`, and where its Weight was given. While a
    /// Feed has no Weight, this names its first listing, and nothing reads it.
    seen: HashMap<Url, (usize, &'static str)>,
}

impl FeedList {
    /// Add one listing from `origin`, merging it with any earlier listing
    /// of the same URL.
    fn add(&mut self, listing: Listing, origin: &Origin<'_>) -> Result<()> {
        let Listing { url, weight } = listing;
        let Some((index, weight_from)) = self.seen.get_mut(&url) else {
            self.seen
                .insert(url.clone(), (self.feeds.len(), origin.describe()));
            self.feeds.push(Feed {
                url,
                weight: weight.map(|(w, _)| w),
            });
            return Ok(());
        };
        let recorded = &mut self.feeds[*index].weight;
        match (*recorded, weight) {
            (Some(existing), Some((w, span))) if existing != w => Err(FeedWeightError {
                src: origin.named(),
                span,
                help: format!(
                    "this feed is already listed with weight {existing} {weight_from}; each feed takes a single weight"
                ),
            }
            .into()),
            // A Weight wins over a listing that gave none.
            (None, Some((w, _))) => {
                *recorded = Some(w);
                *weight_from = origin.describe();
                Ok(())
            }
            // The same Weight again, or a listing with no opinion on it.
            _ => Ok(()),
        }
    }
}

/// One parsed `URL [WEIGHT]` listing. The Weight keeps the span of its
/// token in the origin's text, so a later conflict can point at it.
struct Listing {
    url: Url,
    weight: Option<(NonZeroUsize, SourceSpan)>,
}

/// `range`, a byte range within one listing, shifted by `base` into its
/// origin's text.
fn shifted(base: usize, range: &Range<usize>) -> SourceSpan {
    (base + range.start..base + range.end).into()
}

/// Whitespace-separated tokens of `line`, each with the byte offset it
/// starts at, so diagnostics can point at the exact offending token.
fn tokens_with_offsets(line: &str) -> Vec<(usize, &str)> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (i, c) in line.char_indices() {
        match (start, c.is_whitespace()) {
            (None, false) => start = Some(i),
            (Some(s), true) => {
                tokens.push((s, &line[s..i]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        tokens.push((s, &line[s..]));
    }
    tokens
}

/// Parse one `URL [WEIGHT]` listing found at `base` in `origin`.
///
/// Tokenization happens before URL parsing on purpose: `Url::parse`
/// percent-encodes interior spaces, so handing it the whole line would
/// silently swallow the weight as part of the URL path.
fn parse_listing(line: &str, origin: &Origin<'_>, base: usize) -> Result<Listing> {
    let tokens = tokens_with_offsets(line);
    let trimmed_end = tokens.last().map_or(0, |&(off, tok)| off + tok.len());
    let Some(&(first_start, url_token)) = tokens.first() else {
        return Err(FeedUrlError {
            src: origin.named(),
            span: shifted(base, &(0..line.len())),
            help: "expected a feed URL".to_string(),
        }
        .into());
    };

    let url = Url::parse(url_token).map_err(|e| FeedUrlError {
        src: origin.named(),
        // The whole line is suspect when its first token is not a URL.
        span: shifted(base, &(first_start..trimmed_end)),
        help: e.to_string(),
    })?;

    let weight = match tokens.len() {
        1 => None,
        2 => {
            let (w_start, w_token) = tokens[1];
            let span = shifted(base, &(w_start..w_start + w_token.len()));
            let weight = w_token.parse::<NonZeroUsize>().map_err(|e| FeedWeightError {
                src: origin.named(),
                span,
                help: format!(
                    "the weight after a URL must be a positive integer ({e}); omit it for the default unweighted behavior"
                ),
            })?;
            Some((weight, span))
        }
        _ => {
            let (extra_start, _) = tokens[1];
            return Err(FeedWeightError {
                src: origin.named(),
                span: shifted(base, &(extra_start..trimmed_end)),
                help: "expected `URL [WEIGHT]`: at most one integer weight may follow the URL"
                    .to_string(),
            }
            .into());
        }
    };

    Ok(Listing { url, weight })
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, path::Path};

    use hegel::generators;
    use miette::SourceSpan;
    use url::Url;

    use super::{Feed, UrlsFile, parse};
    use crate::error::{FeedWeightError, OpenringError};

    #[expect(
        clippy::unnecessary_wraps,
        reason = "every caller hands it straight to parse, which takes an Option"
    )]
    fn file(text: &str) -> Option<UrlsFile<'_>> {
        Some(UrlsFile {
            path: Path::new("urls.txt"),
            text,
        })
    }

    fn feed(url: &str, weight: Option<usize>) -> Feed {
        Feed {
            url: Url::parse(url).unwrap(),
            weight: weight.and_then(NonZeroUsize::new),
        }
    }

    fn cli(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    // The text a diagnostic's label covers.
    fn labeled<'a>(source: &'a str, span: &SourceSpan) -> &'a str {
        &source[span.offset()..span.offset() + span.len()]
    }

    #[test]
    fn skips_comments_blank_lines_and_whitespace() {
        let feeds = parse(
            &[],
            file(
                "https://first.example/\n\
                 \x20  https://second.example   \n\
                 \x20https://first.example   \n\
                 # a hash comment\n\
                 // a double‑slash comment\n\
                 \n",
            ),
        )
        .unwrap();
        assert_eq!(
            feeds,
            [
                feed("https://first.example/", None),
                feed("https://second.example/", None)
            ]
        );
    }

    // Round trip: any mix of valid `URL [WEIGHT]` lines, comments, blanks,
    // and stray whitespace parses back to exactly the Feeds written, in the
    // order each URL first appears.
    #[hegel::test]
    fn the_urls_file_round_trips(tc: hegel::TestCase) {
        use std::fmt::Write as _;

        let n = tc.draw(generators::integers::<usize>().max_value(20));
        let mut expected: Vec<Feed> = Vec::new();
        let mut text = String::new();
        for _ in 0..n {
            let url = Url::parse(&tc.draw(generators::urls())).expect("generated URL is valid");
            // A repeated URL repeats its recorded Weight: conflicting Weights
            // are an error with their own property.
            let weight = if let Some(f) = expected.iter().find(|f| f.url == url) {
                f.weight
            } else {
                let weight = tc
                    .draw(generators::optional(
                        generators::integers::<usize>().min_value(1),
                    ))
                    .and_then(NonZeroUsize::new);
                expected.push(Feed {
                    url: url.clone(),
                    weight,
                });
                weight
            };
            match weight {
                Some(w) => writeln!(text, "  {url} {w}  "),
                None => writeln!(text, "  {url}  "),
            }
            .expect("writing to a String cannot fail");
            if tc.draw(generators::booleans()) {
                text.push_str(&tc.draw(hegel::one_of!(
                    generators::just("# comment\n".to_string()),
                    generators::just("// comment\n".to_string()),
                    generators::just("\n".to_string()),
                    generators::just("   \n".to_string()),
                )));
            }
        }

        let parsed = parse(&[], file(&text));
        if expected.is_empty() {
            assert!(matches!(parsed, Err(OpenringError::FeedMissing)));
        } else {
            assert_eq!(parsed.unwrap(), expected);
        }
    }

    // Wherever a URL is listed twice with different Weights, in `-s`
    // values, the file, or one of each, the error points at the second
    // Weight and names the first.
    #[hegel::test]
    fn conflicting_weights_point_at_the_second_weight(tc: hegel::TestCase) {
        let url = Url::parse(&tc.draw(generators::urls())).expect("generated URL is valid");
        let first = tc.draw(generators::integers::<usize>().min_value(1));
        let second = tc.draw(generators::integers::<usize>().min_value(1));
        tc.assume(first != second);
        // Where each listing goes: `-s` values are read before the file, so
        // the first listing is in the file only when both are.
        let first_in_cli = tc.draw(generators::booleans());
        let second_in_cli = first_in_cli && tc.draw(generators::booleans());

        let first_line = format!("{url} {first}");
        let second_line = format!("{url} {second}");
        let mut values = Vec::new();
        let mut text = String::new();
        for (line, in_cli) in [(&first_line, first_in_cli), (&second_line, second_in_cli)] {
            if in_cli {
                values.push(line.clone());
            } else {
                text.push_str(line);
                text.push('\n');
            }
        }

        let err = parse(&values, file(&text)).unwrap_err();
        let OpenringError::FeedWeightError(FeedWeightError { src, span, help }) = err else {
            panic!("expected FeedWeightError, got {err:?}");
        };
        assert_eq!(labeled(src.inner(), &span), second.to_string());
        assert!(help.contains(&format!("weight {first} ")), "help: {help}");
    }

    // The Feed list keeps the order in which each URL first appears, `-s`
    // values first, whatever repeats follow.
    #[hegel::test]
    fn feeds_keep_the_order_they_first_appear(tc: hegel::TestCase) {
        let pool = [
            "https://a.example/",
            "https://b.example/",
            "https://c.example/",
            "https://d.example/",
        ];
        let pick = || generators::sampled_from(pool.to_vec());
        let in_cli = tc.draw(generators::vecs(pick()).max_size(6));
        let in_file = tc.draw(generators::vecs(pick()).max_size(6));
        tc.assume(!in_cli.is_empty() || !in_file.is_empty());

        let text = in_file
            .iter()
            .fold(String::new(), |text, u| text + u + "\n");
        let feeds = parse(&cli(&in_cli), file(&text)).unwrap();

        let mut expected: Vec<&str> = Vec::new();
        for url in in_cli.iter().chain(&in_file) {
            if !expected.contains(url) {
                expected.push(url);
            }
        }
        let got: Vec<&str> = feeds.iter().map(|f| f.url.as_str()).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn a_weight_wins_over_a_listing_without_one_in_either_order() {
        let feeds = parse(
            &cli(&["https://b.example/ 3", "https://c.example/"]),
            file(
                "https://a.example/ 7\n\
                 https://a.example/ 7\n\
                 https://b.example/\n\
                 https://c.example/ 4\n",
            ),
        )
        .unwrap();
        assert_eq!(
            feeds,
            [
                feed("https://b.example/", Some(3)),
                feed("https://c.example/", Some(4)),
                feed("https://a.example/", Some(7)),
            ]
        );
    }

    #[test]
    fn nothing_listed_is_an_error() {
        assert!(matches!(parse(&[], None), Err(OpenringError::FeedMissing)));
        assert!(matches!(
            parse(&[], file("# only a comment\n")),
            Err(OpenringError::FeedMissing)
        ));
    }

    #[test]
    fn a_bad_url_points_at_its_line() {
        // The same text appears first inside a comment; the span must point
        // at the real line, not the first substring match in the file.
        let err = parse(&[], file("# not a url\nnot a url\n")).unwrap_err();
        let OpenringError::FeedUrlError(e) = err else {
            panic!("expected FeedUrlError, got {err:?}");
        };
        assert_eq!(e.span.offset(), "# not a url\n".len());
        assert_eq!(e.span.len(), "not a url".len());
    }

    #[test]
    fn a_bad_weight_points_at_its_token() {
        for (value, token) in [
            ("https://valid.example/ abc", "abc"),
            ("https://valid.example/ 0", "0"),
            ("https://valid.example/ -3", "-3"),
            // Everything after the URL, when more than one token follows.
            ("https://valid.example/ 7 9", "7 9"),
        ] {
            for (values, text) in [
                (cli(&[value]), String::new()),
                (vec![], format!("{value}\n")),
            ] {
                let err = parse(&values, file(&text)).unwrap_err();
                let OpenringError::FeedWeightError(e) = err else {
                    panic!("{value:?}: expected FeedWeightError, got {err:?}");
                };
                assert_eq!(labeled(e.src.inner(), &e.span), token, "{value:?}");
            }
        }
    }

    #[test]
    fn an_empty_value_is_a_bad_url() {
        assert!(matches!(
            parse(&cli(&[""]), None),
            Err(OpenringError::FeedUrlError(_))
        ));
    }

    // The parser must accept or reject, never panic, whatever a value holds.
    #[hegel::test]
    fn any_value_parses_or_fails_cleanly(tc: hegel::TestCase) {
        let value = tc.draw(generators::text());
        let _ = parse(std::slice::from_ref(&value), None);
        let _ = parse(&[], file(&value));
    }
}
