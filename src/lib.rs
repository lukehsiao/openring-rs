pub mod args;
pub mod cache;
pub mod error;
pub mod feedfetcher;

use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    path::Path,
    sync::Arc,
};

use feed_rs::model::{Entry, Feed, Link};
use indicatif::{ProgressBar, ProgressStyle};
use jiff::{Timestamp, civil::Date, tz::TimeZone};
use miette::NamedSource;
use serde::Serialize;
use tera::Tera;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};
use url::Url;
use yansi::Paint;

use crate::{
    args::Args,
    cache::{Cache, CachePath, StoreExt},
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

/// Resolve a possibly-relative URL `href` against the feed’s base `feed_url`.
/// Expects `href` to have a leading `/`.
pub(crate) fn resolve_href(
    feed_url: &Url,
    href: &str,
) -> std::result::Result<Url, url::ParseError> {
    match Url::parse(href) {
        Ok(u) => Ok(u),
        Err(url::ParseError::RelativeUrlWithoutBase) => {
            // Prepend the origin (scheme + authority) of the feed URL.
            Url::parse(&format!(
                "{}{}",
                feed_url.origin().ascii_serialization(),
                href
            ))
        }
        Err(e) => Err(e),
    }
}

/// Parse the file into a vector of URLs.
fn parse_urls_from_file(path: &Path) -> Result<HashSet<Url>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    reader
        .lines()
        // Allow '#' or "//" comments in the urls file
        .filter(|l| {
            let line = l.as_ref().unwrap();
            let trimmed = line.trim();
            !(trimmed.starts_with('#') || trimmed.starts_with("//") || trimmed.is_empty())
        })
        .map(|line| {
            let line = &line.unwrap();
            let line = line.trim();
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

/// Fetch every configured feed, render the most recent articles through the
/// template, and print the result to stdout.
///
/// # Errors
///
/// Returns an error if no feed URLs are given, a URL file cannot be read or
/// holds an invalid URL, the template file cannot be read, the cache cannot be
/// written, a feed entry has no usable link, or the template fails to render.
pub async fn run(args: Args) -> Result<()> {
    debug!(?args);
    let cache = cache::load_cache(&args, CachePath::Default).unwrap_or_default();
    let cache = Arc::new(cache);

    let mut urls = args.url;

    if let Some(path) = args.url_file {
        let file_urls = parse_urls_from_file(&path)?;
        urls.extend(file_urls);
    }

    if urls.is_empty() {
        return Err(OpenringError::FeedMissing);
    }

    // Deduplicate here, too, in case urls are provided in args + file.
    let urls: Vec<Url> = {
        let unique: HashSet<Url> = urls.into_iter().collect();
        unique.into_iter().collect()
    };

    let feeds = get_feeds_from_urls(&urls, &cache).await;

    if let Some(cache_path) = cache::get_cache_path()
        && !args.no_cache
    {
        cache.store(cache_path)?;
    }

    let template = fs::read_to_string(&args.template_file)?;
    let articles = select_articles(feeds, args.per_source, args.num_articles, args.before)?;

    let mut context = tera::Context::new();
    context.insert("articles", &articles);
    // TODO: this validation of the template should come before all the time spent fetching feeds.
    let output = Tera::one_off(&template, &context, true)?;
    write_output(io::stdout().lock(), &output)
}

/// Write the rendered output to `w`, followed by a newline.
///
/// A closed pipe (e.g. `openring ... | head`) is treated as success: the
/// reader has everything it wants, and Unix convention is to exit quietly
/// rather than panic the way `println!` does on EPIPE.
fn write_output(mut w: impl Write, output: &str) -> Result<()> {
    match writeln!(w, "{output}").and_then(|()| w.flush()) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => Ok(result?),
    }
}

/// Build the sorted, truncated list of articles to render from the fetched feeds.
///
/// `per_source` caps how many entries each feed contributes, `before` drops
/// anything published at or after that date (interpreted in the system
/// timezone), and `num_articles` caps the final newest-first list.
fn select_articles(
    feeds: Vec<(Feed, Url)>,
    per_source: usize,
    num_articles: usize,
    before: Option<Date>,
) -> Result<Vec<Article>> {
    let mut articles = Vec::new();
    for (feed, url) in feeds {
        let source_title = resolve_source_title(&feed, &url);
        let source_link = resolve_source_link(&feed, &url)?;
        for entry in feed.entries.iter().take(per_source) {
            if let Some(article) = build_article(entry, &url, &source_title, &source_link, before)?
            {
                articles.push(article);
            }
        }
    }

    articles.sort_unstable_by(|a, b| a.timestamp.cmp(&b.timestamp).reverse());
    articles.truncate(num_articles);
    Ok(articles)
}

/// The href of the first link explicitly tagged `rel="alternate"`, if any.
fn find_alternate_link(links: &[Link]) -> Option<&str> {
    links
        .iter()
        .find(|l| l.rel.as_deref() == Some("alternate"))
        .map(|l| l.href.as_str())
}

/// The display title for a feed: its declared title, or the fetch URL's domain
/// when the feed omits the title or leaves it blank.
fn resolve_source_title(feed: &Feed, feed_url: &Url) -> String {
    match &feed.title {
        Some(t) if !t.content.is_empty() => t.content.clone(),
        _ => feed_url.domain().unwrap().to_owned(),
    }
}

/// The canonical homepage link for a feed.
///
/// Prefers the title's `src`, then an `alternate` link, then any link that is
/// not the feed's own `self` link, and finally falls back to the feed URL.
fn resolve_source_link(feed: &Feed, feed_url: &Url) -> Result<Url> {
    if let Some(src) = feed.title.as_ref().and_then(|t| t.src.as_ref()) {
        return Ok(resolve_href(feed_url, src)?);
    }
    if let Some(href) = find_alternate_link(&feed.links) {
        return Ok(resolve_href(feed_url, href)?);
    }
    // Ignore "self" rels, which usually link back to the feed itself.
    if let Some(link) = feed.links.iter().find(|l| l.rel.as_deref() != Some("self")) {
        return Ok(resolve_href(feed_url, &link.href)?);
    }
    warn!(
        source = feed_url.as_str(),
        "feed is missing root link: falling back to rss feed url."
    );
    Ok(feed_url.clone())
}

/// The best link for an entry, resolved against the feed origin.
///
/// Prefers an `alternate` link, falling back to the first link present.
/// Returns `Ok(None)` when a link exists but cannot be parsed (the entry is
/// then skipped); errors only when the entry has no links at all.
fn resolve_entry_link(entry: &Entry, feed_url: &Url) -> Result<Option<Url>> {
    let href = match find_alternate_link(&entry.links) {
        Some(href) => href,
        None => match entry.links.first() {
            Some(link) => link.href.as_str(),
            None => return Err(OpenringError::FeedBadTitle(feed_url.to_string())),
        },
    };
    Ok(resolve_href(feed_url, href).ok())
}

/// The entry's summary text, preferring an explicit `<summary>` and falling back
/// to the content body. `None` when the entry carries neither.
fn raw_summary(entry: &Entry) -> Option<&str> {
    entry
        .summary
        .as_ref()
        .map(|s| s.content.as_str())
        .or_else(|| entry.content.as_ref().and_then(|c| c.body.as_deref()))
}

/// Strip unsafe markup and decode HTML entities, returning trimmed text.
fn sanitize_html(raw: &str) -> String {
    let mut safe = String::new();
    html_escape::decode_html_entities_to_string(ammonia::clean(raw), &mut safe);
    safe.trim().to_string()
}

/// Build a renderable [`Article`] from a feed entry.
///
/// Returns `Ok(None)` when the entry lacks a usable link, title, or date, or
/// when it falls at or after `before`. Errors only when the entry has no links
/// or its date is out of representable range.
fn build_article(
    entry: &Entry,
    feed_url: &Url,
    source_title: &str,
    source_link: &Url,
    before: Option<Date>,
) -> Result<Option<Article>> {
    let (Some(link), Some(title), Some(date)) = (
        resolve_entry_link(entry, feed_url)?,
        entry.title.as_ref().map(|t| &t.content),
        entry.published.or(entry.updated),
    ) else {
        warn!(
            entry_links = ?entry.links,
            entry_title = ?entry.title,
            entry_published = ?entry.published,
            entry_updated = ?entry.updated,
            source = feed_url.as_str(),
            "skipping entry: must have link, title, and a date."
        );
        return Ok(None);
    };

    let timestamp = Timestamp::from_second(date.timestamp())?;
    if let Some(before) = before
        && timestamp > before.to_zoned(TimeZone::system())?.timestamp()
    {
        return Ok(None);
    }

    let summary = raw_summary(entry).map_or_else(
        || {
            info!(?link, ?source_link, "no summary or content provided.");
            String::new()
        },
        sanitize_html,
    );

    Ok(Some(Article {
        link,
        title: title.clone(),
        summary,
        source_link: source_link.clone(),
        source_title: source_title.to_owned(),
        timestamp,
    }))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, io::Write};
    use url::Url;

    use feed_rs::model::{Entry, Feed, Link};
    use hegel::generators;

    use super::{
        build_article, find_alternate_link, parse_urls_from_file, raw_summary, resolve_entry_link,
        resolve_href, resolve_source_link, resolve_source_title, sanitize_html, select_articles,
        write_output,
    };

    // A writer that always fails with the given kind, standing in for a stdout
    // that has gone away.
    struct FailingWriter(std::io::ErrorKind);

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(self.0, "stub failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_output_appends_newline() {
        let mut buf = Vec::new();
        write_output(&mut buf, "hello").unwrap();
        assert_eq!(buf, b"hello\n");
    }

    #[test]
    fn write_output_treats_broken_pipe_as_success() {
        assert!(write_output(FailingWriter(std::io::ErrorKind::BrokenPipe), "x").is_ok());
    }

    #[test]
    fn write_output_propagates_other_io_errors() {
        assert!(matches!(
            write_output(FailingWriter(std::io::ErrorKind::PermissionDenied), "x"),
            Err(crate::error::OpenringError::IoError(_))
        ));
    }

    // Generates a base URL and a random path fragment. The property asserts that:
    // * If `href` is already absolute, the result equals `Url::parse(href)`.
    // * If `href` is relative, the result's origin matches the base URL's origin.
    #[hegel::test]
    fn resolve_href_preserves_origin(tc: hegel::TestCase) {
        let scheme = tc.draw(hegel::one_of!(
            generators::just("http".to_string()),
            generators::just("https".to_string())
        ));
        let host = tc.draw(
            generators::from_regex(r"(?:[a-zA-Z0-9-]{1,63}\.)+[a-zA-Z]{2,63}").fullmatch(true),
        );
        let port = tc.draw(generators::integers::<u16>().min_value(80).max_value(65535));
        let rel_path = tc.draw(generators::from_regex(r"/[a-zA-Z0-9_/-]{1,30}").fullmatch(true));

        let base_str = format!("{scheme}://{host}:{port}");
        let base_url = Url::parse(&base_str);

        // The naive host regex can still emit invalid punycode; skip those inputs.
        tc.assume(base_url.is_ok());
        let base_url = base_url.unwrap();

        let absolute = format!("{base_str}/{rel_path}");
        let resolved_abs = resolve_href(&base_url, &absolute).unwrap();
        // Parse the generated string so we compare canonical `Url`s, not raw strings.
        let expected_abs = Url::parse(&absolute).unwrap();
        assert_eq!(resolved_abs, expected_abs);

        let resolved_rel = resolve_href(&base_url, &rel_path).unwrap();
        // Origin (scheme + host + port) must be identical.
        assert_eq!(resolved_rel.origin(), base_url.origin());
        // Path component should be exactly the relative fragment prefixed with '/'.
        assert_eq!(resolved_rel.path(), rel_path);
    }

    #[test]
    fn parse_urls_ignores_comments_and_blank_lines_and_whitespace() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();

        // Valid URLs
        writeln!(tmp, "https://first.example/").unwrap();
        writeln!(tmp, "   https://second.example   ").unwrap(); // leading/trailing spaces
        writeln!(tmp, " https://first.example   ").unwrap(); // duplicate (even missing trailing slash)

        // Comments
        writeln!(tmp, "# a hash comment").unwrap();
        writeln!(tmp, "// a double‑slash comment").unwrap();

        // Blank line
        writeln!(tmp).unwrap();

        let parsed = parse_urls_from_file(tmp.path()).unwrap();

        let expected = HashSet::from([
            Url::parse("https://first.example/").unwrap(),
            Url::parse("https://second.example/").unwrap(),
        ]);
        assert_eq!(parsed, expected);
    }

    #[test]
    fn parse_urls_reports_invalid_url() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "https://valid.example/").unwrap();
        writeln!(tmp, "not a url").unwrap();

        // An unparseable line yields a diagnostic error rather than being
        // silently dropped.
        assert!(matches!(
            parse_urls_from_file(tmp.path()),
            Err(crate::error::OpenringError::FeedUrlError(_))
        ));
    }

    #[test]
    fn resolve_href_propagates_non_relative_parse_errors() {
        let base = Url::parse("https://example.com/").unwrap();
        // A scheme with an empty host is a hard parse error, not a relative
        // reference, so it should propagate instead of being prepended.
        assert!(resolve_href(&base, "http://").is_err());
    }

    const FEED_URL: &str = "https://example.com/feed.xml";

    fn feed_url() -> Url {
        Url::parse(FEED_URL).unwrap()
    }

    // Parse a feed fixture the same way production does.
    fn parse_feed(xml: &str) -> Feed {
        feed_rs::parser::parse(xml.as_bytes()).unwrap()
    }

    // Pair a parsed feed with the URL it was "fetched" from.
    fn feed(url: &str, xml: &str) -> (Feed, Url) {
        (parse_feed(xml), Url::parse(url).unwrap())
    }

    // Wrap entry XML in a minimal Atom feed (title + homepage link present).
    fn atom(entries: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>Example Blog</title>
                <link href="https://example.com/"/>
                <updated>2020-01-01T00:00:00Z</updated>
                {entries}
            </feed>"#
        )
    }

    // The first entry of a single-entry Atom fixture, for entry-level helpers.
    fn first_entry(entry_xml: &str) -> Entry {
        parse_feed(&atom(entry_xml))
            .entries
            .into_iter()
            .next()
            .unwrap()
    }

    fn link(href: &str, rel: Option<&str>) -> Link {
        Link {
            href: href.to_owned(),
            rel: rel.map(str::to_owned),
            media_type: None,
            href_lang: None,
            title: None,
            length: None,
        }
    }

    #[test]
    fn find_alternate_link_picks_the_alternate_rel() {
        let links = vec![
            link("https://example.com/feed.xml", Some("self")),
            link("https://example.com/home", Some("alternate")),
        ];
        assert_eq!(
            find_alternate_link(&links),
            Some("https://example.com/home")
        );
        assert_eq!(find_alternate_link(&links[..1]), None);
    }

    #[test]
    fn resolve_source_title_prefers_title_then_domain() {
        // A declared, non-empty title is used verbatim.
        assert_eq!(
            resolve_source_title(&parse_feed(&atom("")), &feed_url()),
            "Example Blog"
        );

        // An empty or absent title falls back to the fetch URL's domain. Both an
        // empty <title> and a missing one resolve the same way, so this holds
        // however feed-rs chooses to represent the blank case.
        let blank = parse_feed(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title></title>
                <link href="https://example.com/"/>
                <updated>2020-01-01T00:00:00Z</updated>
            </feed>"#,
        );
        assert_eq!(resolve_source_title(&blank, &feed_url()), "example.com");
    }

    #[test]
    fn resolve_source_link_prefers_alternate_then_other_then_feed_url() {
        // An explicit alternate link wins over a self link.
        let alt = parse_feed(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>t</title>
                <link rel="self" href="https://example.com/feed.xml"/>
                <link rel="alternate" href="https://example.com/home"/>
                <updated>2020-01-01T00:00:00Z</updated>
            </feed>"#,
        );
        assert_eq!(
            resolve_source_link(&alt, &feed_url()).unwrap().as_str(),
            "https://example.com/home"
        );

        // With no alternate, any link that is not the feed's own `self` is used.
        let other = parse_feed(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>t</title>
                <link rel="self" href="https://example.com/feed.xml"/>
                <link rel="related" href="https://example.com/related"/>
                <updated>2020-01-01T00:00:00Z</updated>
            </feed>"#,
        );
        assert_eq!(
            resolve_source_link(&other, &feed_url()).unwrap().as_str(),
            "https://example.com/related"
        );
    }

    #[test]
    fn resolve_source_link_falls_back_to_feed_url_without_usable_links() {
        // A titleless feed whose only link is `self` has nothing else to point
        // at, so the feed URL itself is used. This also guards the old panic on
        // a missing <title> (it used to `unwrap`).
        let bare = parse_feed(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <link rel="self" href="https://example.com/feed.xml"/>
                <updated>2020-01-01T00:00:00Z</updated>
            </feed>"#,
        );
        assert_eq!(resolve_source_link(&bare, &feed_url()).unwrap(), feed_url());
    }

    #[test]
    fn resolve_entry_link_resolves_relative_and_errors_without_links() {
        // An absolute link is returned unchanged.
        let abs = first_entry(
            r#"<entry>
                <title>t</title>
                <link href="https://other.example/abs"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>"#,
        );
        assert_eq!(
            resolve_entry_link(&abs, &feed_url())
                .unwrap()
                .unwrap()
                .as_str(),
            "https://other.example/abs"
        );

        // A relative link is resolved against the feed origin.
        let rel = first_entry(
            r#"<entry>
                <title>t</title>
                <link href="/rel-path"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>"#,
        );
        assert_eq!(
            resolve_entry_link(&rel, &feed_url())
                .unwrap()
                .unwrap()
                .as_str(),
            "https://example.com/rel-path"
        );

        // An entry with no links at all is a hard error.
        let none = first_entry(
            r"<entry>
                <title>t</title>
                <published>2020-01-01T00:00:00Z</published>
            </entry>",
        );
        assert!(matches!(
            resolve_entry_link(&none, &feed_url()),
            Err(crate::error::OpenringError::FeedBadTitle(_))
        ));
    }

    #[test]
    fn raw_summary_prefers_summary_then_content_then_none() {
        let summary = first_entry(
            r#"<entry>
                <title>t</title>
                <link href="https://example.com/a"/>
                <published>2020-01-01T00:00:00Z</published>
                <summary>the summary</summary>
            </entry>"#,
        );
        assert_eq!(raw_summary(&summary), Some("the summary"));

        let content_only = first_entry(
            r#"<entry>
                <title>t</title>
                <link href="https://example.com/a"/>
                <published>2020-01-01T00:00:00Z</published>
                <content type="html">&lt;p&gt;body&lt;/p&gt;</content>
            </entry>"#,
        );
        assert!(raw_summary(&content_only).unwrap().contains("body"));

        let neither = first_entry(
            r#"<entry>
                <title>t</title>
                <link href="https://example.com/a"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>"#,
        );
        assert_eq!(raw_summary(&neither), None);
    }

    #[test]
    fn sanitize_html_strips_scripts_decodes_entities_and_trims() {
        let out = sanitize_html("  <p>Safe</p><script>alert(1)</script> Tom &amp; Jerry  ");
        // ammonia removes the <script>; html_escape decodes &amp; back to '&'.
        assert!(!out.contains("script"));
        assert!(out.contains("Safe"));
        assert!(out.contains("Tom & Jerry"));
        assert_eq!(out, out.trim());
    }

    #[test]
    fn build_article_assembles_fields_and_applies_filters() {
        use jiff::civil::date;

        let source_link = Url::parse("https://example.com/").unwrap();
        let future = first_entry(
            r#"<entry>
                <title>Future Post</title>
                <link href="https://example.com/future"/>
                <published>2024-01-01T00:00:00Z</published>
                <summary>hello</summary>
            </entry>"#,
        );

        // With no cutoff, every field flows through, including the source info.
        let built = build_article(&future, &feed_url(), "Src", &source_link, None)
            .unwrap()
            .unwrap();
        assert_eq!(built.title, "Future Post");
        assert_eq!(built.link.as_str(), "https://example.com/future");
        assert_eq!(built.summary, "hello");
        assert_eq!(built.source_title, "Src");
        assert_eq!(built.source_link, source_link);

        // An entry at or after `before` is dropped.
        assert!(
            build_article(
                &future,
                &feed_url(),
                "Src",
                &source_link,
                Some(date(2022, 1, 1))
            )
            .unwrap()
            .is_none()
        );

        // An entry missing its date is skipped rather than erroring.
        let dateless = first_entry(
            r#"<entry>
                <title>Dateless</title>
                <link href="https://example.com/x"/>
                <summary>s</summary>
            </entry>"#,
        );
        assert!(
            build_article(&dateless, &feed_url(), "Src", &source_link, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn select_articles_extracts_basic_fields() {
        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>First Post</title>
                    <link href="https://example.com/first"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>Hello world</summary>
                </entry>"#,
            ),
        )];

        let articles = select_articles(feeds, 1, 10, None).unwrap();
        assert_eq!(articles.len(), 1);
        let a = &articles[0];
        assert_eq!(a.link.as_str(), "https://example.com/first");
        assert_eq!(a.title, "First Post");
        assert_eq!(a.summary, "Hello world");
        assert_eq!(a.source_title, "Example Blog");
        assert_eq!(a.source_link.as_str(), "https://example.com/");
    }

    #[test]
    fn select_articles_caps_entries_per_source() {
        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>Newest</title>
                    <link href="https://example.com/3"/>
                    <published>2022-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Middle</title>
                    <link href="https://example.com/2"/>
                    <published>2021-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Oldest</title>
                    <link href="https://example.com/1"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>"#,
            ),
        )];

        // per_source = 1 keeps only the first entry in document order.
        let articles = select_articles(feeds, 1, 10, None).unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Newest");
    }

    #[test]
    fn select_articles_sorts_newest_first_and_caps_total() {
        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>Oldest</title>
                    <link href="https://example.com/1"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Newest</title>
                    <link href="https://example.com/3"/>
                    <published>2022-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Middle</title>
                    <link href="https://example.com/2"/>
                    <published>2021-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>"#,
            ),
        )];

        // num_articles = 2 keeps the two newest, sorted newest-first.
        let articles = select_articles(feeds, 10, 2, None).unwrap();
        assert_eq!(articles.len(), 2);
        assert_eq!(articles[0].title, "Newest");
        assert_eq!(articles[1].title, "Middle");
        assert!(articles[0].timestamp > articles[1].timestamp);
    }

    #[test]
    fn select_articles_drops_entries_at_or_after_before_date() {
        use jiff::civil::date;

        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>Old</title>
                    <link href="https://example.com/old"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>New</title>
                    <link href="https://example.com/new"/>
                    <published>2024-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>"#,
            ),
        )];

        // The two-year gap dwarfs any timezone offset, so the boundary is stable.
        let articles = select_articles(feeds, 10, 10, Some(date(2022, 1, 1))).unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Old");
    }

    #[tokio::test]
    async fn run_fetches_and_renders_end_to_end() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use super::run;
        use crate::args::Args;

        let server = MockServer::start().await;
        let body = r#"<?xml version="1.0"?>
            <rss version="2.0">
                <channel>
                    <title>Mock Feed</title>
                    <link>https://example.com/</link>
                    <description>desc</description>
                    <item>
                        <title>Mock Article</title>
                        <link>https://example.com/mock</link>
                        <description>summary</description>
                        <pubDate>Tue, 10 Jun 2003 04:00:00 GMT</pubDate>
                    </item>
                </channel>
            </rss>"#;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let mut template = tempfile::NamedTempFile::new().unwrap();
        template
            .write_all(b"{% for a in articles %}{{ a.title }}\n{% endfor %}")
            .unwrap();

        let args = Args {
            url: vec![Url::parse(&server.uri()).unwrap()],
            template_file: template.path().to_path_buf(),
            // no_cache keeps the run from touching the real on-disk cache.
            no_cache: true,
            num_articles: 3,
            per_source: 1,
            ..Default::default()
        };

        assert!(run(args).await.is_ok());
    }
}
