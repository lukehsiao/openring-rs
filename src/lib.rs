pub mod args;
pub mod cache;
pub mod error;
pub mod feedfetcher;

use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
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

/// Resolve a possibly-relative URL `href` against the URL of the feed it came
/// from, following RFC 3986: absolute hrefs stand alone, root-relative hrefs
/// resolve against the feed's origin, and path-relative or protocol-relative
/// hrefs resolve against the feed URL itself.
pub(crate) fn resolve_href(
    feed_url: &Url,
    href: &str,
) -> std::result::Result<Url, url::ParseError> {
    feed_url.join(href)
}

/// Parse the file into a set of feed URLs.
///
/// Blank lines and lines starting with `#` or `//` are ignored. The first
/// invalid URL fails the parse with a diagnostic spanning that exact line.
fn parse_urls_from_file(path: &Path) -> Result<HashSet<Url>> {
    let file_src = fs::read_to_string(path)?;

    let mut urls = HashSet::new();
    let mut offset = 0;
    for raw_line in file_src.split_inclusive('\n') {
        let line = raw_line.trim();
        if !(line.is_empty() || line.starts_with('#') || line.starts_with("//")) {
            match Url::parse(line) {
                Ok(url) => {
                    urls.insert(url);
                }
                Err(e) => {
                    let start = offset + (raw_line.len() - raw_line.trim_start().len());
                    return Err(FeedUrlError {
                        src: NamedSource::new(path.to_string_lossy(), file_src.clone()),
                        span: (start..start + line.len()).into(),
                        help: e.to_string(),
                    }
                    .into());
                }
            }
        }
        offset += raw_line.len();
    }
    Ok(urls)
}

// Get all feeds from URLs concurrently.
//
// Skips feeds if there are errors. Shows progress. Errors only when the
// shared HTTP client cannot be built at all.
async fn get_feeds_from_urls(urls: &[Url], cache: &Arc<Cache>) -> Result<Vec<(Feed, Url)>> {
    // The progress bar's message lists the fetches still in flight.
    fn show_pending(pb: &ProgressBar, pending: &HashSet<&Url>) {
        pb.set_message(
            pending
                .iter()
                .map(|u| u.as_str())
                .collect::<Vec<&str>>()
                .join(", "),
        );
    }

    // One client for the whole run, so every fetch shares a connection pool
    // instead of paying for TLS setup per feed.
    let client = feedfetcher::build_client()?;

    let pb = ProgressBar::new(urls.len() as u64).with_style(
        ProgressStyle::with_template("{prefix:>8} [{bar}] {human_pos}/{human_len}: {wide_msg}")
            .unwrap(),
    );
    pb.set_prefix("Fetching".bold().to_string());

    let mut join_set = JoinSet::new();
    let mut pending_urls: HashSet<&Url> = HashSet::from_iter(urls);

    show_pending(&pb, &pending_urls);

    for url in urls {
        let cache_clone = Arc::clone(cache);
        // reqwest::Client is a cheap handle to the shared pool.
        let client_clone = client.clone();
        let url_clone = url.clone();
        join_set.spawn(async move {
            let fetch_result = url_clone.fetch_feed(&client_clone, &cache_clone).await;
            (url_clone, fetch_result)
        });
    }
    let mut feeds = Vec::new();

    while let Some(result) = join_set.join_next().await {
        pb.inc(1);
        match result {
            Ok((url, Ok(feed))) => {
                pending_urls.remove(&url);
                show_pending(&pb, &pending_urls);
                pb.println(format!("{:>8} {url}", "Fetched".bold().green()));
                feeds.push((feed, url));
            }
            Ok((url, Err(e))) => {
                pending_urls.remove(&url);
                show_pending(&pb, &pending_urls);
                pb.println(format!("{:>8} {url} ({e})", "Error".bold().red()));
            }
            Err(e) => {
                // A fetch task that panicked or was aborted. The URL is lost
                // with the task, so it cannot be removed from the pending
                // list, but the failure must not be silent.
                warn!(error=%e, "feed fetch task failed");
                pb.println(format!(
                    "{:>8} fetch task failed ({e})",
                    "Error".bold().red()
                ));
            }
        }
    }

    pb.finish_and_clear();
    Ok(feeds)
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

    // Read and parse the template before anything else: a wrong path or a
    // syntax error should fail in milliseconds, not after fetching every feed.
    // The name's .html suffix is what turns on Tera's autoescaping.
    let template_name = "template.html";
    let mut tera = Tera::default();
    tera.add_raw_template(template_name, &fs::read_to_string(&args.template_file)?)?;

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

    let feeds = get_feeds_from_urls(&urls, &cache).await?;

    if let Some(cache_path) = cache::get_cache_path()
        && !args.no_cache
    {
        cache.store(cache_path)?;
    }

    let articles = select_articles(feeds, args.per_source, args.num_articles, args.before)?;

    let mut context = tera::Context::new();
    context.insert("articles", &articles);
    let output = tera.render(template_name, &context)?;
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
/// Each feed contributes its `per_source` most recent qualifying entries,
/// judged by publication date rather than the order the feed lists them in.
/// `before` drops anything published after that date (interpreted in the
/// system timezone) before the cap applies, and `num_articles` caps the final
/// newest-first list.
fn select_articles(
    feeds: Vec<(Feed, Url)>,
    per_source: usize,
    num_articles: usize,
    before: Option<Date>,
) -> Result<Vec<Article>> {
    // Convert the cutoff to an instant once: midnight at the start of
    // `before` in the system timezone.
    let cutoff = before
        .map(|date| date.to_zoned(TimeZone::system()).map(|z| z.timestamp()))
        .transpose()?;

    let mut articles = Vec::new();
    for (feed, url) in feeds {
        let source_title = resolve_source_title(&feed, &url);
        let source_link = resolve_source_link(&feed, &url)?;
        let mut from_feed = Vec::new();
        for entry in &feed.entries {
            if let Some(article) = build_article(entry, &url, &source_title, &source_link, cutoff)?
            {
                from_feed.push(article);
            }
        }
        from_feed.sort_unstable_by_key(|a| std::cmp::Reverse(a.timestamp));
        from_feed.truncate(per_source);
        articles.append(&mut from_feed);
    }

    articles.sort_unstable_by_key(|a| std::cmp::Reverse(a.timestamp));
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

/// The display title for a feed: its declared title, or the fetch URL's host
/// (domain or IP address) when the feed omits the title or leaves it blank.
fn resolve_source_title(feed: &Feed, feed_url: &Url) -> String {
    match &feed.title {
        Some(t) if !t.content.is_empty() => t.content.clone(),
        // Hostless URLs (e.g. file:) can't be fetched anyway, so the full URL
        // is an adequate last resort.
        _ => feed_url
            .host_str()
            .unwrap_or_else(|| feed_url.as_str())
            .to_owned(),
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

/// The best link for an entry, resolved against the feed URL.
///
/// Prefers an `alternate` link, falling back to the first link present.
/// `None` when the entry has no links or the href cannot be parsed; the
/// caller skips such entries like any other incomplete entry.
fn resolve_entry_link(entry: &Entry, feed_url: &Url) -> Option<Url> {
    let href = find_alternate_link(&entry.links)
        .or_else(|| entry.links.first().map(|link| link.href.as_str()))?;
    resolve_href(feed_url, href).ok()
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

/// Decode HTML entities, then strip unsafe markup, returning trimmed HTML
/// that is safe to embed even through the template's `| safe` filter.
///
/// Decoding must come first: cleaning and then decoding would turn harmless
/// entity-encoded text like `&lt;script&gt;` back into live markup.
fn sanitize_html(raw: &str) -> String {
    let decoded = html_escape::decode_html_entities(raw);
    ammonia::clean(&decoded).trim().to_string()
}

/// Reduce feed-supplied text like titles to plain text: entities decoded,
/// every tag stripped, script/style bodies dropped, and the remaining text
/// kept entity-escaped so it is safe to embed in HTML.
fn sanitize_text(raw: &str) -> String {
    let decoded = html_escape::decode_html_entities(raw);
    let mut builder = ammonia::Builder::empty();
    builder.clean_content_tags(["script", "style"].into_iter().collect());
    builder.clean(&decoded).to_string().trim().to_string()
}

/// Build a renderable [`Article`] from a feed entry.
///
/// Returns `Ok(None)` when the entry lacks a usable link, title, or date, or
/// when it falls at or after the `cutoff` instant. Errors only when the
/// entry's date is out of representable range.
fn build_article(
    entry: &Entry,
    feed_url: &Url,
    source_title: &str,
    source_link: &Url,
    cutoff: Option<Timestamp>,
) -> Result<Option<Article>> {
    let (Some(link), Some(title), Some(date)) = (
        resolve_entry_link(entry, feed_url),
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
    if let Some(cutoff) = cutoff
        && timestamp >= cutoff
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
        // Titles are feed-controlled and the default template embeds them
        // with `| safe`, so they get the same boundary treatment as summaries.
        title: sanitize_text(title),
        summary,
        source_link: source_link.clone(),
        source_title: sanitize_text(source_title),
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
        let host = tc.draw(generators::domains());
        let port = tc.draw(generators::integers::<u16>().min_value(80).max_value(65535));
        // A second leading '/' would make the href protocol-relative, which
        // names its own host and is covered by its own test below.
        let rel_path =
            tc.draw(generators::from_regex(r"/[a-zA-Z0-9_-][a-zA-Z0-9_/-]{0,29}").fullmatch(true));

        let base_str = format!("{scheme}://{host}:{port}");
        let base_url = Url::parse(&base_str).expect("generated domain is a valid host");

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

    // Round-trip: any mix of valid URL lines, comments, blanks, and stray
    // whitespace parses back to exactly the set of URLs written.
    #[hegel::test(test_cases = 25)]
    fn parse_urls_round_trips_urls_through_a_file(tc: hegel::TestCase) {
        let n = tc.draw(generators::integers::<usize>().min_value(0).max_value(20));
        let mut expected = HashSet::new();
        let mut lines = Vec::new();
        for _ in 0..n {
            let url = Url::parse(&tc.draw(generators::urls())).expect("generated URL is valid");
            lines.push(format!("  {url}  "));
            expected.insert(url);
            if tc.draw(generators::booleans()) {
                lines.push(tc.draw(hegel::one_of!(
                    generators::just("# comment".to_string()),
                    generators::just("// comment".to_string()),
                    generators::just(String::new()),
                    generators::just("   ".to_string()),
                )));
            }
        }

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for line in &lines {
            writeln!(tmp, "{line}").unwrap();
        }

        assert_eq!(parse_urls_from_file(tmp.path()).unwrap(), expected);
    }

    #[test]
    fn parse_urls_errors_instead_of_panicking_on_invalid_utf8() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&[0xFF, 0xFE, b'\n']).unwrap();
        // A urls file in a non-UTF-8 encoding is a user mistake, not a crash.
        assert!(matches!(
            parse_urls_from_file(tmp.path()),
            Err(crate::error::OpenringError::IoError(_))
        ));
    }

    #[test]
    fn parse_urls_diagnostic_points_at_the_offending_line() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        // The same text appears first inside a comment; the span must point
        // at the real line, not the first substring match in the file.
        writeln!(tmp, "# not a url").unwrap();
        writeln!(tmp, "not a url").unwrap();

        let err = parse_urls_from_file(tmp.path()).unwrap_err();
        let crate::error::OpenringError::FeedUrlError(feed_url_error) = err else {
            panic!("expected FeedUrlError, got {err:?}");
        };
        assert_eq!(feed_url_error.span.offset(), "# not a url\n".len());
        assert_eq!(feed_url_error.span.len(), "not a url".len());
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

    // Path-relative hrefs (no leading '/') resolve against the feed URL's
    // directory, never by string-concatenating onto the origin, which used to
    // produce mangled hosts like `https://example.compage.html`.
    #[hegel::test]
    fn resolve_href_resolves_path_relative_hrefs(tc: hegel::TestCase) {
        let dir = tc.draw(generators::from_regex(r"(/[a-z0-9]{1,8}){0,3}/").fullmatch(true));
        let rel =
            tc.draw(generators::from_regex(r"[a-z0-9]{1,8}(/[a-z0-9]{1,8}){0,2}").fullmatch(true));
        let base = Url::parse(&format!("https://example.com{dir}feed.xml")).unwrap();
        let resolved = resolve_href(&base, &rel).unwrap();
        assert_eq!(resolved.origin(), base.origin());
        assert_eq!(resolved.path(), format!("{dir}{rel}"));
    }

    #[test]
    fn resolve_href_handles_protocol_relative_hrefs() {
        let base = Url::parse("https://example.com/feed.xml").unwrap();
        // A protocol-relative href names its own host; only the scheme comes
        // from the feed URL.
        assert_eq!(
            resolve_href(&base, "//other.example/x").unwrap().as_str(),
            "https://other.example/x"
        );
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

    // A titleless feed must get a fallback title for any URL a feed can be
    // fetched from, including IP-address hosts where `Url::domain()` is None.
    #[hegel::test]
    fn resolve_source_title_falls_back_for_any_http_host(tc: hegel::TestCase) {
        use hegel::generators::Generator;
        let host = tc.draw(hegel::one_of!(
            generators::domains(),
            generators::ip_addresses().v4(),
            // IPv6 hosts go in brackets inside a URL.
            generators::ip_addresses().v6().map(|ip| format!("[{ip}]")),
        ));
        let url = Url::parse(&format!("http://{host}/feed.xml")).expect("generated host is valid");

        let blank = parse_feed(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title></title>
                <link href="https://example.com/"/>
                <updated>2020-01-01T00:00:00Z</updated>
            </feed>"#,
        );
        let title = resolve_source_title(&blank, &url);
        assert_eq!(title, url.host_str().unwrap());
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
    fn resolve_entry_link_resolves_relative_and_skips_linkless() {
        // An absolute link is returned unchanged.
        let abs = first_entry(
            r#"<entry>
                <title>t</title>
                <link href="https://other.example/abs"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>"#,
        );
        assert_eq!(
            resolve_entry_link(&abs, &feed_url()).unwrap().as_str(),
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
            resolve_entry_link(&rel, &feed_url()).unwrap().as_str(),
            "https://example.com/rel-path"
        );

        // An entry with no links has no resolvable link; the caller skips it.
        let none = first_entry(
            r"<entry>
                <title>t</title>
                <published>2020-01-01T00:00:00Z</published>
            </entry>",
        );
        assert_eq!(resolve_entry_link(&none, &feed_url()), None);
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
    fn sanitize_html_strips_scripts_keeps_text_and_trims() {
        let out = sanitize_html("  <p>Safe</p><script>alert(1)</script> Tom &amp; Jerry  ");
        // ammonia removes the <script> and keeps text entity-escaped, so the
        // result stays safe even through the template's `| safe` filter.
        assert!(!out.contains("script"));
        assert!(out.contains("Safe"));
        assert!(out.contains("Tom &amp; Jerry"));
        assert_eq!(out, out.trim());
    }

    // No input, however it encodes its markup, may come out of sanitization
    // with a live <script> tag. Decoding entities after cleaning used to turn
    // harmless text like "&lt;script&gt;" back into real markup, which the
    // default template then embedded raw via `| safe`.
    #[hegel::test]
    fn sanitize_html_never_emits_live_markup(tc: hegel::TestCase) {
        let payload = tc.draw(hegel::one_of!(
            generators::just("<script>alert(1)</script>".to_string()),
            generators::just("&lt;script&gt;alert(1)&lt;/script&gt;".to_string()),
            generators::just("&amp;lt;script&amp;gt;alert(1)&amp;lt;/script&amp;gt;".to_string()),
            generators::text(),
        ));
        let prefix = tc.draw(generators::text().max_size(16));
        let suffix = tc.draw(generators::text().max_size(16));
        let out = sanitize_html(&format!("{prefix}{payload}{suffix}"));
        assert!(
            !out.to_lowercase().contains("<script"),
            "live script tag in sanitized output: {out:?}"
        );
    }

    #[test]
    fn build_article_sanitizes_title_and_source_title() {
        let source_link = Url::parse("https://example.com/").unwrap();
        // The XML parser decodes the entities, so the in-memory title holds
        // real <script> markup, exactly as a hostile feed would deliver it.
        let entry = first_entry(
            r#"<entry>
                <title>&lt;script&gt;alert(1)&lt;/script&gt;Hello</title>
                <link href="https://example.com/x"/>
                <published>2020-01-01T00:00:00Z</published>
                <summary>s</summary>
            </entry>"#,
        );
        let article = build_article(
            &entry,
            &feed_url(),
            "<script>evil()</script>Src",
            &source_link,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(article.title, "Hello");
        assert_eq!(article.source_title, "Src");
    }

    #[test]
    fn build_article_assembles_fields_and_applies_filters() {
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

        // An entry at or after the cutoff instant is dropped.
        assert!(
            build_article(
                &future,
                &feed_url(),
                "Src",
                &source_link,
                Some("2022-01-01T00:00:00Z".parse().unwrap())
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
    fn select_articles_skips_malformed_entries_instead_of_aborting() {
        // One linkless entry in one feed must not take down the whole run;
        // it is skipped exactly like entries missing a title or date.
        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>No Link</title>
                    <published>2021-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Good</title>
                    <link href="https://example.com/good"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>"#,
            ),
        )];

        let articles = select_articles(feeds, 10, 10, None).unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Good");
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

        // per_source = 1 keeps only the most recent entry.
        let articles = select_articles(feeds, 1, 10, None).unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Newest");
    }

    #[test]
    fn select_articles_picks_most_recent_per_source_regardless_of_feed_order() {
        // Entries listed oldest-first: per_source must still pick by date,
        // not document order. Nothing in RSS/Atom promises newest-first.
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
                    <title>Middle</title>
                    <link href="https://example.com/2"/>
                    <published>2021-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Newest</title>
                    <link href="https://example.com/3"/>
                    <published>2022-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>"#,
            ),
        )];

        let articles = select_articles(feeds, 1, 10, None).unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Newest");
    }

    #[test]
    fn select_articles_per_source_counts_only_qualifying_entries() {
        use jiff::civil::date;

        // The newest entry falls after --before; it must not use up the
        // per-source budget and shadow the older entry that qualifies.
        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>New</title>
                    <link href="https://example.com/new"/>
                    <published>2024-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Old</title>
                    <link href="https://example.com/old"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>"#,
            ),
        )];

        let articles = select_articles(feeds, 1, 10, Some(date(2022, 1, 1))).unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Old");
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
    fn select_articles_drops_entry_published_exactly_at_the_cutoff() {
        use jiff::{civil::date, tz::TimeZone};

        // An article published at the very instant `before` begins is not
        // "before" that date and must be excluded.
        let cutoff_date = date(2022, 1, 1);
        let cutoff = cutoff_date
            .to_zoned(TimeZone::system())
            .unwrap()
            .timestamp();
        let feeds = vec![feed(
            FEED_URL,
            &atom(&format!(
                r#"<entry>
                    <title>At Cutoff</title>
                    <link href="https://example.com/at"/>
                    <published>{cutoff}</published>
                    <summary>x</summary>
                </entry>"#
            )),
        )];

        let articles = select_articles(feeds, 10, 10, Some(cutoff_date)).unwrap();
        assert!(
            articles.is_empty(),
            "boundary article was kept: {articles:?}"
        );
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
    async fn run_fails_on_bad_template_before_fetching_any_feed() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use super::run;
        use crate::args::Args;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        // A template with a syntax error: rendering could never succeed, so
        // the run must fail before spending time on the network.
        let mut template = tempfile::NamedTempFile::new().unwrap();
        template.write_all(b"{% for a in %}").unwrap();

        let args = Args {
            url: vec![Url::parse(&server.uri()).unwrap()],
            template_file: template.path().to_path_buf(),
            no_cache: true,
            ..Default::default()
        };
        assert!(run(args).await.is_err());

        let received = server.received_requests().await.unwrap();
        assert!(
            received.is_empty(),
            "feeds were fetched despite a template that can never render"
        );
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
