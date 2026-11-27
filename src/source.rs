//! Parsing raw Feed bytes and projecting them onto a Source and its
//! Articles, and the rules every Article keeps: its text is safe to embed
//! in HTML, and a Source holds its Articles newest first.
//!
//! This is the only module that touches `feed_rs`. Once bytes parse as a
//! feed document, nothing in it can fail the run: a bad link falls back,
//! and an Entry that cannot become an Article is skipped and counted.

use feed_rs::model::{Entry, Feed as Document, Link};
use jiff::{Timestamp, ZonedDateTime};
use serde::Serialize;
use thiserror::Error;
use tracing::{debug, info, warn};
use url::{Position, Url};

use crate::feed_list::Feed;

/// Raw bytes did not parse as an RSS, Atom, or JSON Feed document.
#[derive(Debug, Error)]
#[error(transparent)]
pub(crate) struct ParseError(feed_rs::parser::ParseFeedError);

/// A fetched Feed with every complete Entry as an Article. Each Article
/// carries the Source's display title and homepage link.
#[derive(Debug)]
pub(crate) struct Source {
    feed: Feed,
    /// Newest first by [`article_order`].
    articles: Vec<Article>,
}

impl Source {
    pub(crate) fn feed(&self) -> &Feed {
        &self.feed
    }

    /// The Feed and its Articles, newest first by [`article_order`].
    pub(crate) fn into_parts(self) -> (Feed, Vec<Article>) {
        (self.feed, self.articles)
    }
}

/// One Article as templates see it. Every text field is safe to embed in
/// HTML through `| safe`, as the README promises: titles in quoted
/// attributes too, the summary only as element content.
#[derive(Serialize, Debug)]
pub(crate) struct Article {
    link: Url,
    title: String,
    /// `None` until a summary is found, from the feed or from the
    /// Article's own page.
    #[serde(serialize_with = "summary_or_empty")]
    summary: Option<String>,
    source_link: Url,
    source_title: String,
    timestamp: Timestamp,
}

impl Article {
    pub(crate) fn link(&self) -> &Url {
        &self.link
    }

    pub(crate) fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    pub(crate) fn has_summary(&self) -> bool {
        self.summary.is_some()
    }

    /// Give the Article a summary from plain text, such as text derived from
    /// its page. The text is escaped here, so markup in it shows as text.
    pub(crate) fn fill_summary(&mut self, plain_text: &str) {
        self.summary = Some(html_escape::encode_text(plain_text).into_owned());
    }
}

/// Templates see a missing summary as an empty string. Filters such as
/// `newlines_to_br` fail on a null, so a `None` would break the summary
/// pipeline that `in.html` and the README show.
#[expect(
    clippy::ref_option,
    reason = "serde passes the field to serialize_with by reference"
)]
fn summary_or_empty<S: serde::Serializer>(
    summary: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(summary.as_deref().unwrap_or_default())
}

#[cfg(test)]
impl Source {
    /// A Source for `feed` holding `articles`, sorted as projection sorts
    /// them.
    pub(crate) fn fixture(feed: Feed, mut articles: Vec<Article>) -> Self {
        articles.sort_unstable_by(article_order);
        Self { feed, articles }
    }

    pub(crate) fn articles(&self) -> &[Article] {
        &self.articles
    }
}

#[cfg(test)]
impl Article {
    /// An Article at `link` with no summary, from the Source whose homepage
    /// is `source_link`. `title` must already be safe to embed.
    pub(crate) fn fixture(source_link: &Url, title: &str, link: Url, timestamp: Timestamp) -> Self {
        Self {
            link,
            title: title.to_owned(),
            summary: None,
            source_link: source_link.clone(),
            source_title: source_link.host_str().unwrap_or_default().to_owned(),
            timestamp,
        }
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    pub(crate) fn source_link(&self) -> &Url {
        &self.source_link
    }

    pub(crate) fn source_title(&self) -> &str {
        &self.source_title
    }
}

/// Parse raw Feed bytes and project the document onto its [`Source`].
///
/// Warns once when the document lists Entries but none of them is complete,
/// so a Feed that yields nothing usable does not vanish silently.
///
/// # Errors
///
/// Returns [`ParseError`] when `body` is not a valid feed document.
pub(crate) fn parse(body: &[u8], feed: &Feed) -> Result<Source, ParseError> {
    let document = feed_rs::parser::parse(body).map_err(ParseError)?;
    Ok(project(&document, feed.clone()))
}

/// Project a parsed `document` for `feed` onto its Source.
fn project(document: &Document, feed: Feed) -> Source {
    let title = resolve_source_title(document, &feed.url);
    let link = resolve_source_link(document, &feed.url);
    let mut articles: Vec<Article> = document
        .entries
        .iter()
        .filter_map(|entry| build_article(entry, &feed.url, &title, &link))
        .collect();
    // One actionable signal instead of a warning per malformed entry.
    if articles.is_empty() && !document.entries.is_empty() {
        warn!(
            source = feed.url.as_str(),
            skipped = document.entries.len(),
            "feed contributed no articles: its entries are missing a link, title, or date"
        );
    }
    articles.sort_unstable_by(article_order);
    Source { feed, articles }
}

/// Newest first, with ties broken by link so the output is identical no
/// matter what order the feeds finished fetching in.
pub(crate) fn article_order(a: &Article, b: &Article) -> std::cmp::Ordering {
    b.timestamp
        .cmp(&a.timestamp)
        .then_with(|| a.link.cmp(&b.link))
}

/// The content links among `links`: the explicit `alternate` ones first,
/// then the ones with no `rel`, each group in document order. Atom treats a
/// missing `rel` as `alternate`, and RSS writes none, so both name the
/// content itself. Comment links carry a target, and edit, enclosure, hub,
/// paging, and `self` links carry another `rel`; they all point somewhere
/// else.
fn content_hrefs(links: &[Link]) -> impl Iterator<Item = &str> {
    let alternate = links.iter().filter(|l| {
        l.rel
            .as_deref()
            .is_some_and(|rel| rel.eq_ignore_ascii_case("alternate"))
    });
    let bare = links.iter().filter(|l| l.rel.is_none());
    alternate
        .chain(bare)
        .filter(|l| l.target.is_none())
        .map(|l| l.href.as_str())
}

/// `href` resolved against the feed URL, if it names a web page other than
/// the feed itself.
///
/// Only http and https pass. A feed controls these links, and templates
/// embed them through `| safe`, so a `javascript:` or `data:` link would
/// run script in the reader's page. An empty href or a bare fragment
/// resolves to the feed itself, which is never the page a reader wants.
fn usable_link(href: &str, feed_url: &Url) -> Option<Url> {
    let url = feed_url.join(href.trim()).ok()?;
    let is_web = matches!(url.scheme(), "http" | "https");
    let is_feed = url[..Position::AfterQuery] == feed_url[..Position::AfterQuery];
    if !is_web || is_feed {
        return None;
    }
    attribute_safe(&url)
}

/// `url` with nothing that could end a quoted attribute, or `None` when
/// that cannot be done.
///
/// The url crate escapes `"`, `<`, `>`, and spaces in a path, query, or
/// fragment, and `'` in an http or https query, but keeps `'` in a path or
/// fragment, which would end a single-quoted `href='…'`.
/// `%27` names the same resource. A host may still hold a quote of either
/// kind: the url crate accepts one there, and decodes `%27` back to `'`.
/// No real host does.
fn attribute_safe(url: &Url) -> Option<Url> {
    let url = Url::parse(&url.as_str().replace('\'', "%27")).ok()?;
    (!url.as_str().contains(['\'', '"'])).then_some(url)
}

/// The display title for a feed, sanitized: its declared title, or the
/// fetch URL's host (domain or IP address) when the feed omits the title or
/// it holds nothing to show.
fn resolve_source_title(document: &Document, feed_url: &Url) -> String {
    document
        .title
        .as_ref()
        .and_then(|t| visible_text(&t.content))
        // Hostless URLs (e.g. file:) can't be fetched anyway, so the full
        // URL is an adequate last resort.
        .unwrap_or_else(|| sanitize_text(feed_url.host_str().unwrap_or(feed_url.as_str())))
}

/// `raw` sanitized (see [`sanitize_text`]), or `None` when nothing a reader
/// could see is left: only whitespace, no-break spaces, or markup.
fn visible_text(raw: &str) -> Option<String> {
    let text = sanitize_text(raw);
    let shown = html_escape::decode_html_entities(&text);
    (!shown.trim().is_empty()).then_some(text)
}

/// The canonical homepage link for a feed.
///
/// Prefers the title's `src`, then the feed's content links (see
/// [`content_hrefs`]), and finally falls back to the feed URL. A link that
/// is not usable (see [`usable_link`]) is skipped, and the next one in that
/// order applies.
fn resolve_source_link(document: &Document, feed_url: &Url) -> Url {
    let src = document.title.as_ref().and_then(|t| t.src.as_deref());
    if let Some(url) = src
        .into_iter()
        .chain(content_hrefs(&document.links))
        .find_map(|href| usable_link(href, feed_url))
    {
        return url;
    }
    warn!(
        source = feed_url.as_str(),
        "feed is missing root link: falling back to rss feed url."
    );
    // The user configured this URL, so it is trusted, but it reaches
    // templates the same way as a feed's own links.
    attribute_safe(feed_url).unwrap_or_else(|| feed_url.clone())
}

/// The best link for an entry, resolved against the feed URL: its first
/// usable content link (see [`content_hrefs`] and [`usable_link`]).
/// Comment links are never chosen: they point at discussion of the post,
/// not the post itself. `None` when no link is usable; the caller skips
/// such entries like any other incomplete entry.
fn resolve_entry_link(entry: &Entry, feed_url: &Url) -> Option<Url> {
    content_hrefs(&entry.links).find_map(|href| usable_link(href, feed_url))
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
/// kept entity-escaped so it is safe to embed in HTML, quoted attributes
/// included.
fn sanitize_text(raw: &str) -> String {
    let decoded = html_escape::decode_html_entities(raw);
    let mut builder = ammonia::Builder::empty();
    builder.clean_content_tags(["script", "style"].into_iter().collect());
    let text = builder.clean(&decoded).to_string();
    // Ammonia escapes `&`, `<`, and `>`, but not quotes, which would end a
    // quoted attribute such as `title="{{ article.title | safe }}"`.
    text.trim().replace('"', "&quot;").replace('\'', "&#39;")
}

/// Build a renderable [`Article`] from a feed entry. `source_title` must
/// already be sanitized.
///
/// Returns `None` exactly when the entry is incomplete: missing a usable
/// link, a title a reader can see, or a usable date. Selection policy (date
/// cutoff, per-source caps) belongs to [`crate::select`].
fn build_article(
    entry: &Entry,
    feed_url: &Url,
    source_title: &str,
    source_link: &Url,
) -> Option<Article> {
    let (Some(link), Some(title), Some(timestamp)) = (
        resolve_entry_link(entry, feed_url),
        entry.title.as_ref().and_then(|t| visible_text(&t.content)),
        entry
            .published
            .as_ref()
            .or(entry.updated.as_ref())
            .map(ZonedDateTime::timestamp),
    ) else {
        // Routine for third-party feeds and recurring every run, so the
        // per-entry detail stays at debug; project warns once per feed
        // when nothing at all is usable.
        debug!(
            entry_links = ?entry.links,
            entry_title = ?entry.title,
            entry_published = ?entry.published,
            entry_updated = ?entry.updated,
            source = feed_url.as_str(),
            "skipping entry: must have link, title, and a date."
        );
        return None;
    };

    // A summary with nothing safe left after sanitizing is missing too: the
    // reader would see nothing either way, and the page may say more.
    let summary = raw_summary(entry)
        .map(sanitize_html)
        .filter(|summary| !summary.is_empty());
    if summary.is_none() {
        info!(
            ?link,
            ?source_link,
            "feed has no usable summary or content."
        );
    }

    Some(Article {
        link,
        // Already sanitized: titles are feed-controlled and the default
        // template embeds them with `| safe`.
        title,
        summary,
        source_link: source_link.clone(),
        source_title: source_title.to_owned(),
        timestamp,
    })
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use feed_rs::model::Link;
    use hegel::generators;
    use url::Url;

    use super::{ParseError, Source, article_order, parse, project};
    use crate::{feed_list::Feed, test_support::warnings_logged};

    const FEED_URL: &str = "https://example.com/feed.xml";

    const SAMPLE_ENTRY: &str = r#"<entry>
        <title>Sample Post</title>
        <link href="https://example.com/sample"/>
        <published>2020-01-01T00:00:00Z</published>
    </entry>"#;

    fn feed_url() -> Url {
        Url::parse(FEED_URL).unwrap()
    }

    fn feed(url: &str) -> Feed {
        Feed {
            url: Url::parse(url).unwrap(),
            weight: None,
        }
    }

    // Parse raw Feed XML as if fetched from the unweighted Feed at `url`.
    fn parse_at(url: &str, xml: &str) -> Source {
        parse(xml.as_bytes(), &feed(url)).unwrap()
    }

    fn parse_xml(xml: &str) -> Source {
        parse_at(FEED_URL, xml)
    }

    // Wrap entry XML in a minimal Atom feed with a title and homepage link.
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

    // Escape XML attribute characters and preserve ASCII whitespace verbatim.
    fn xml_escape_attr(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&apos;"),
                '\t' => out.push_str("&#x9;"),
                '\n' => out.push_str("&#xA;"),
                '\r' => out.push_str("&#xD;"),
                _ => out.push(c),
            }
        }
        out
    }

    #[test]
    fn parse_valid_feed_projects_source_and_preserves_feed() {
        let weighted_feed = Feed {
            url: feed_url(),
            weight: NonZeroUsize::new(3),
        };
        let xml = atom(
            r#"<entry>
                <title>First Post</title>
                <link href="https://example.com/first"/>
                <published>2020-01-01T00:00:00Z</published>
                <summary>Hello world</summary>
            </entry>"#,
        );

        let source = parse(xml.as_bytes(), &weighted_feed).expect("valid Atom feed parses");

        assert_eq!(source.feed(), &weighted_feed);
        let [article] = source.articles() else {
            panic!("expected one article: {:?}", source.articles());
        };
        assert_eq!(article.link().as_str(), "https://example.com/first");
        assert_eq!(article.title(), "First Post");
        assert_eq!(article.summary(), Some("Hello world"));
        assert_eq!(article.source_title(), "Example Blog");
        assert_eq!(article.source_link().as_str(), "https://example.com/");
        assert_eq!(article.timestamp().to_string(), "2020-01-01T00:00:00Z");
    }

    #[test]
    fn parse_rejects_non_feed_bytes() {
        let err: ParseError =
            parse(b"<html><body>not a feed</body></html>", &feed(FEED_URL)).unwrap_err();
        let err_ref: &dyn std::error::Error = &err;
        assert_ne!(err_ref.to_string(), "");
    }

    // Whatever links an Entry carries, its Article link is a web page that
    // is not the Feed itself, and holds nothing that could end a quoted
    // attribute. A Feed controls these links and the bundled template embeds
    // them through `| safe`, so any other scheme or a stray quote could run
    // script in the reader's page.
    #[hegel::test]
    fn entry_links_are_only_web_pages_other_than_the_feed(tc: hegel::TestCase) {
        use std::fmt::Write as _;

        let href = || {
            hegel::one_of!(
                generators::sampled_from(
                    [
                        "https://example.com/post",
                        "http://example.com/post",
                        "/post",
                        "post#frag",
                        "",
                        "#top",
                        "feed.xml",
                        "https://example.com/feed.xml#x",
                        "javascript:alert(1)",
                        "JavaScript:alert(1)",
                        "javascript:\"><img src=x onerror=alert(1)>",
                        "data:text/html,<script>alert(1)</script>",
                        "mailto:me@example.com",
                        "ftp://example.com/post",
                        "http://[",
                        "/x'/onmouseover='alert(1)",
                        "/a#f'g",
                        "/p?q=a'b",
                        "//a\"b.example/post",
                        "https://a'b.example/post",
                        "https://a\"onmouseover=\"alert(1)\".example/",
                    ]
                    .map(str::to_owned)
                    .to_vec()
                ),
                // Match the XML 1.0 Char production so every draw parses as Atom.
                generators::text()
                    .min_codepoint(0x20)
                    .include_characters("\t\n\r")
                    .exclude_characters("\u{fffe}\u{ffff}")
                    .max_size(24)
            )
        };
        let rel = || {
            generators::sampled_from(vec![
                None,
                Some("alternate"),
                Some("Alternate"),
                Some("replies"),
                Some("edit"),
                Some("self"),
                Some("hub"),
            ])
        };
        let n = tc.draw(generators::integers::<usize>().max_value(4));
        let mut links = String::new();
        for _ in 0..n {
            let escaped_href = xml_escape_attr(&tc.draw(href()));
            match tc.draw(rel()) {
                Some(rel_val) => {
                    write!(links, r#"<link rel="{rel_val}" href="{escaped_href}"/>"#).unwrap();
                }
                None => {
                    write!(links, r#"<link href="{escaped_href}"/>"#).unwrap();
                }
            }
        }
        let entry = format!(
            "<entry><title>t</title>{links}<published>2020-01-01T00:00:00Z</published></entry>"
        );
        let source = parse_xml(&atom(&entry));

        if let Some(article) = source.articles().first() {
            let url = article.link();
            assert!(matches!(url.scheme(), "http" | "https"), "{url}");
            let mut bare = url.clone();
            bare.set_fragment(None);
            assert_ne!(bare, feed_url(), "{url}");
            assert!(
                !url.as_str().contains(
                    |c: char| matches!(c, '"' | '\'' | '<' | '>') || c.is_ascii_whitespace()
                ),
                "{url}"
            );
        }
    }

    #[test]
    fn a_script_link_never_becomes_the_article_link() {
        // The only link runs script, so the Entry has no usable link.
        let only_script = parse_xml(&atom(
            r#"<entry>
                <title>Script Only</title>
                <link rel="alternate" href="javascript:&quot;&gt;&lt;img src=x onerror=alert(1)&gt;"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>"#,
        ));
        assert!(only_script.articles().is_empty());

        // A safe link after it wins instead.
        let with_page = parse_xml(&atom(
            r#"<entry>
                <title>Fallback</title>
                <link rel="alternate" href="javascript:alert(1)"/>
                <link href="https://example.com/post"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>"#,
        ));
        let [article] = with_page.articles() else {
            panic!("expected one article: {:?}", with_page.articles());
        };
        assert_eq!(article.link().as_str(), "https://example.com/post");
    }

    #[test]
    fn an_entry_link_back_to_the_feed_is_not_a_link_to_the_post() {
        let source = parse_xml(&atom(
            r##"<entry>
                <title>Post</title>
                <link rel="alternate" href=""/>
                <link href="#comments"/>
                <link href="https://example.com/post"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>"##,
        ));
        let [article] = source.articles() else {
            panic!("expected one article: {:?}", source.articles());
        };
        assert_eq!(article.link().as_str(), "https://example.com/post");
    }

    // A Feed URL may hold an apostrophe. Links that resolve back to it are
    // still the Feed, not the Article.
    #[test]
    fn a_link_back_to_a_feed_url_with_an_apostrophe_is_still_the_feed() {
        let source = parse_at(
            "https://example.com/it's/feed.xml",
            r##"<?xml version="1.0" encoding="utf-8"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>Example Blog</title>
                <updated>2020-01-01T00:00:00Z</updated>
                <entry>
                    <title>Post</title>
                    <link rel="alternate" href=""/>
                    <link href="#top"/>
                    <link href="feed.xml"/>
                    <link href="post"/>
                    <published>2020-01-01T00:00:00Z</published>
                </entry>
            </feed>"##,
        );
        let [article] = source.articles() else {
            panic!("expected one article: {:?}", source.articles());
        };
        assert_eq!(article.link().as_str(), "https://example.com/it%27s/post");
        // Without a usable homepage link, the fallback Feed URL is escaped too.
        assert_eq!(
            article.source_link().as_str(),
            "https://example.com/it%27s/feed.xml"
        );
    }

    #[test]
    fn a_bad_alternate_falls_back_to_the_next_content_link() {
        let source = parse_xml(&atom(
            r#"<entry>
                <title>Post</title>
                <link rel="alternate" href="http://["/>
                <link href="https://example.com/post"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>"#,
        ));
        let [article] = source.articles() else {
            panic!("expected one article: {:?}", source.articles());
        };
        assert_eq!(article.link().as_str(), "https://example.com/post");
    }

    #[test]
    fn atom_comment_edit_and_enclosure_links_are_never_the_post() {
        let source = parse_xml(&atom(
            r#"<entry>
                <title>No Content Link</title>
                <link rel="replies" href="https://example.com/post/comments"/>
                <link rel="edit" href="https://example.com/post/edit"/>
                <link rel="enclosure" href="https://example.com/post.mp3"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>"#,
        ));
        assert!(source.articles().is_empty());
    }

    #[test]
    fn entry_links_resolve_relative_and_skip_linkless() {
        let source = parse_xml(&atom(
            r#"<entry>
                <title>Absolute</title>
                <link href="https://other.example/abs"/>
                <published>2020-01-02T00:00:00Z</published>
            </entry>
            <entry>
                <title>Relative</title>
                <link href="/rel-path"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>
            <entry>
                <title>Linkless</title>
                <published>2020-01-03T00:00:00Z</published>
            </entry>"#,
        ));
        let links: Vec<&str> = source
            .articles()
            .iter()
            .map(|article| article.link().as_str())
            .collect();
        assert_eq!(
            links,
            ["https://other.example/abs", "https://example.com/rel-path"]
        );
    }

    #[test]
    fn entry_links_ignore_rss_comment_links() {
        // feed-rs lists RSS `<comments>` and `wfw:commentRss` as entry links
        // in document order, so a comments link can come first. The Article
        // must point at the post itself.
        let source = parse_xml(
            r#"<?xml version="1.0"?>
            <rss version="2.0" xmlns:wfw="http://wellformedweb.org/CommentAPI/">
                <channel>
                    <title>t</title>
                    <link>https://example.com/</link>
                    <item>
                        <title>a</title>
                        <comments>https://example.com/a#comments</comments>
                        <wfw:commentRss>https://example.com/a/feed</wfw:commentRss>
                        <link>https://example.com/a</link>
                        <pubDate>Wed, 01 Jan 2020 00:00:00 GMT</pubDate>
                    </item>
                    <item>
                        <title>b</title>
                        <comments>https://example.com/b#comments</comments>
                        <pubDate>Thu, 02 Jan 2020 00:00:00 GMT</pubDate>
                    </item>
                </channel>
            </rss>"#,
        );
        let links: Vec<&str> = source
            .articles()
            .iter()
            .map(|article| article.link().as_str())
            .collect();
        assert_eq!(links, ["https://example.com/a"]);
    }

    #[test]
    fn the_homepage_is_never_a_hub_or_paging_link() {
        let source = parse_xml(&format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>Example Blog</title>
                <link rel="hub" href="https://hub.example/"/>
                <link rel="next" href="https://example.com/feed.xml?page=2"/>
                <link href="https://example.com/"/>
                <updated>2020-01-01T00:00:00Z</updated>
                {SAMPLE_ENTRY}
            </feed>"#
        ));
        assert_eq!(
            source.articles()[0].source_link().as_str(),
            "https://example.com/"
        );
    }

    #[test]
    fn source_link_prefers_alternate_then_plain_then_feed_url() {
        // An explicit alternate link wins over both a self link and an
        // earlier plain <link>.
        let alt = parse_xml(
            r#"<?xml version="1.0"?>
            <rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom">
                <channel>
                    <title>t</title>
                    <atom:link rel="self" href="https://example.com/feed.xml"/>
                    <link>https://example.com/plain</link>
                    <atom:link rel="alternate" href="https://example.com/home"/>
                    <description>d</description>
                    <item>
                        <title>Post</title>
                        <link>https://example.com/post</link>
                        <pubDate>Wed, 01 Jan 2020 00:00:00 GMT</pubDate>
                    </item>
                </channel>
            </rss>"#,
        );
        assert_eq!(
            alt.articles()[0].source_link().as_str(),
            "https://example.com/home"
        );

        // An RSS channel's plain <link> has no rel, and it is the homepage
        // even when a self link comes first.
        let plain = parse_xml(
            r#"<?xml version="1.0"?>
            <rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom">
                <channel>
                    <title>t</title>
                    <atom:link rel="self" href="https://example.com/feed.xml"/>
                    <link>https://example.com/plain</link>
                    <description>d</description>
                    <item>
                        <title>Post</title>
                        <link>https://example.com/post</link>
                        <pubDate>Wed, 01 Jan 2020 00:00:00 GMT</pubDate>
                    </item>
                </channel>
            </rss>"#,
        );
        assert_eq!(
            plain.articles()[0].source_link().as_str(),
            "https://example.com/plain"
        );

        // A related link points somewhere else, so the Feed URL stands in.
        let related = parse_xml(&format!(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>t</title>
                <link rel="self" href="https://example.com/feed.xml"/>
                <link rel="related" href="https://example.com/related"/>
                <updated>2020-01-01T00:00:00Z</updated>
                {SAMPLE_ENTRY}
            </feed>"#
        ));
        assert_eq!(related.articles()[0].source_link(), &feed_url());
    }

    #[test]
    fn source_link_falls_back_to_feed_url_without_usable_links() {
        // A titleless Feed whose only link is `self` has nothing else to
        // point at, so the Feed URL itself is used. This also guards against
        // a panic on a missing <title>.
        let bare = parse_xml(&format!(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <link rel="self" href="https://example.com/feed.xml"/>
                <updated>2020-01-01T00:00:00Z</updated>
                {SAMPLE_ENTRY}
            </feed>"#
        ));
        assert_eq!(bare.articles()[0].source_link(), &feed_url());
    }

    // Unparseable hrefs at any step of the source-link chain never stop
    // projection. The first href that resolves wins, and the Feed URL is
    // the last resort. feed-rs parsers never populate `title.src`, so this
    // test sets it on a parsed document directly and calls `project`.
    #[hegel::test]
    fn project_source_link_skips_unparseable_hrefs(tc: hegel::TestCase) {
        let pick = |good: &'static str| {
            hegel::one_of!(
                generators::just(Some(good)),
                generators::just(Some("http://[")),
                generators::just(Some("javascript:alert(1)")),
                generators::just(None::<&'static str>),
            )
        };
        let src = tc.draw(pick("https://example.com/src"));
        let alternate = tc.draw(pick("https://example.com/alt"));
        let other = tc.draw(pick("https://example.com/other"));

        let mut document = feed_rs::parser::parse(atom(SAMPLE_ENTRY).as_bytes()).unwrap();
        document.links.clear();
        if let Some(title) = document.title.as_mut() {
            title.src = src.map(str::to_owned);
        }
        let make_link = |href: &str, rel: Option<&str>| Link {
            href: href.to_owned(),
            target: None,
            rel: rel.map(str::to_owned),
            media_type: None,
            href_lang: None,
            title: None,
            length: None,
        };
        if let Some(href) = alternate {
            document.links.push(make_link(href, Some("alternate")));
        }
        if let Some(href) = other {
            document.links.push(make_link(href, None));
        }
        let good = |href: Option<&'static str>| href.filter(|h| h.starts_with("https://"));
        let expected = good(src)
            .or(good(alternate))
            .or(good(other))
            .map_or_else(feed_url, |h| Url::parse(h).unwrap());
        let source = project(&document, feed(FEED_URL));
        assert_eq!(source.articles()[0].source_link(), &expected);
    }

    // A title a reader cannot see falls back to the host for a Feed, and
    // makes an Entry incomplete.
    #[test]
    fn invisible_titles_count_as_missing() {
        for title in ["   ", "<b></b>", "<script>x</script>", "&nbsp;"] {
            let feed_json = serde_json::json!({
                "version": "https://jsonfeed.org/version/1.1",
                "title": title,
                "home_page_url": "https://example.com/",
                "items": [
                    {
                        "id": "hidden",
                        "title": title,
                        "url": "https://example.com/hidden",
                        "date_published": "2020-01-02T00:00:00Z",
                    },
                    {
                        "id": "visible",
                        "title": "Visible",
                        "url": "https://example.com/visible",
                        "date_published": "2020-01-01T00:00:00Z",
                    },
                ],
            })
            .to_string();
            let source = parse_xml(&feed_json);
            let [article] = source.articles() else {
                panic!(
                    "expected only the visible entry for {title:?}: {:?}",
                    source.articles()
                );
            };
            assert_eq!(article.title(), "Visible", "{title:?}");
            assert_eq!(article.source_title(), "example.com", "{title:?}");
        }
    }

    // A titleless Feed must get a fallback title for any URL a Feed can be
    // fetched from, including IP-address hosts where `Url::domain()` is None.
    #[hegel::test]
    fn source_title_falls_back_for_any_http_host(tc: hegel::TestCase) {
        use hegel::generators::Generator;
        let host = tc.draw(hegel::one_of!(
            generators::domains(),
            generators::ip_addresses().v4().map(|ip| ip.to_string()),
            // IPv6 hosts go in brackets inside a URL.
            generators::ip_addresses().v6().map(|ip| format!("[{ip}]")),
        ));
        let url_str = format!("http://{host}/feed.xml");
        let url = Url::parse(&url_str).expect("generated host is valid");

        let xml = format!(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <link href="https://example.com/"/>
                <updated>2020-01-01T00:00:00Z</updated>
                {SAMPLE_ENTRY}
            </feed>"#
        );
        let source = parse_at(&url_str, &xml);
        assert_eq!(source.articles()[0].source_title(), url.host_str().unwrap());
    }

    #[test]
    fn an_out_of_range_published_date_falls_back_to_updated() {
        // A five-digit year is past jiff's range in year 9999, so projection
        // uses the Entry's updated date instead.
        let source = parse_xml(
            r#"<?xml version="1.0"?>
            <rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom">
                <channel>
                    <title>t</title>
                    <link>https://example.com/</link>
                    <item>
                        <title>t</title>
                        <link>https://example.com/post</link>
                        <pubDate>01 Jan 10000 00:00:00 GMT</pubDate>
                        <atom:updated>2020-01-01T00:00:00Z</atom:updated>
                    </item>
                </channel>
            </rss>"#,
        );
        let [article] = source.articles() else {
            panic!("expected one article: {:?}", source.articles());
        };
        assert_eq!(article.timestamp().to_string(), "2020-01-01T00:00:00Z");
    }

    #[test]
    fn summary_prefers_summary_then_content_then_none() {
        let source = parse_xml(&atom(
            r#"<entry>
                <title>Both</title>
                <link href="https://example.com/both"/>
                <published>2020-01-03T00:00:00Z</published>
                <summary>the summary</summary>
                <content type="html">&lt;p&gt;body&lt;/p&gt;</content>
            </entry>
            <entry>
                <title>Content Only</title>
                <link href="https://example.com/content"/>
                <published>2020-01-02T00:00:00Z</published>
                <content type="html">&lt;p&gt;body&lt;/p&gt;</content>
            </entry>
            <entry>
                <title>Neither</title>
                <link href="https://example.com/neither"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>"#,
        ));
        let summaries: Vec<Option<&str>> = source
            .articles()
            .iter()
            .map(|article| article.summary())
            .collect();
        assert_eq!(summaries, [Some("the summary"), Some("<p>body</p>"), None]);
    }

    #[test]
    fn summary_sanitization_strips_scripts_keeps_text_and_trims() {
        let source = parse_xml(&atom(
            r#"<entry>
                <title>Post</title>
                <link href="https://example.com/post"/>
                <published>2020-01-01T00:00:00Z</published>
                <summary><![CDATA[  <p>Safe</p><script>alert(1)</script> Tom &amp; Jerry  ]]></summary>
            </entry>"#,
        ));
        let out = source.articles()[0].summary().unwrap();
        // Ammonia removes `<script>` and keeps text entity-escaped, so the
        // summary stays safe through the template's `| safe` filter.
        assert!(!out.contains("script"));
        assert!(out.contains("Safe"));
        assert!(out.contains("Tom &amp; Jerry"));
        assert_eq!(out, out.trim());
    }

    // No input, however it encodes its markup, may come out of sanitization
    // with a live <script> tag. Decoding entities after cleaning would turn
    // harmless text like "&lt;script&gt;" back into real markup, which the
    // default template embeds raw via `| safe`.
    #[hegel::test]
    fn sanitized_summaries_never_emit_live_script_markup(tc: hegel::TestCase) {
        let payload = tc.draw(hegel::one_of!(
            generators::just("<script>alert(1)</script>".to_string()),
            generators::just("&lt;script&gt;alert(1)&lt;/script&gt;".to_string()),
            generators::just("&amp;lt;script&amp;gt;alert(1)&amp;lt;/script&amp;gt;".to_string()),
            generators::text(),
        ));
        let prefix = tc.draw(generators::text().max_size(16));
        let suffix = tc.draw(generators::text().max_size(16));
        let raw = format!("{prefix}{payload}{suffix}");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": "https://jsonfeed.org/version/1.1",
            "title": "Example Blog",
            "home_page_url": "https://example.com/",
            "items": [{
                "id": "1",
                "url": "https://example.com/post",
                "title": "Post",
                "summary": raw,
                "date_published": "2020-01-01T00:00:00Z",
            }],
        }))
        .unwrap();

        let source = parse(&bytes, &feed(FEED_URL)).unwrap();
        if let Some(out) = source.articles()[0].summary() {
            assert!(
                !out.to_lowercase().contains("<script"),
                "live script tag in sanitized output: {out:?}"
            );
        }
    }

    // Sanitized titles hold no character that could open a tag or end a
    // quoted attribute, so they are safe anywhere a template puts them.
    #[hegel::test]
    fn sanitized_titles_never_emit_markup_or_quotes(tc: hegel::TestCase) {
        let payload = tc.draw(hegel::one_of!(
            generators::just("\"><img src=x onerror=alert(1)>".to_string()),
            generators::just("' onmouseover='alert(1)".to_string()),
            generators::just("&quot; onclick=&quot;alert(1)".to_string()),
            generators::just("&lt;script&gt;alert(1)&lt;/script&gt;".to_string()),
            generators::text(),
        ));
        let prefix = tc.draw(generators::text().max_size(16));
        let suffix = tc.draw(generators::text().max_size(16));
        let raw = format!("{prefix}{payload}{suffix}");
        // Add a second plain Entry so the test still checks `source_title`
        // when `parse` drops the hostile Entry for an empty title.
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": "https://jsonfeed.org/version/1.1",
            "title": raw,
            "home_page_url": "https://example.com/",
            "items": [
                {
                    "id": "1",
                    "url": "https://example.com/hostile",
                    "title": raw,
                    "date_published": "2020-01-01T00:00:00Z",
                },
                {
                    "id": "2",
                    "url": "https://example.com/plain",
                    "title": "Plain",
                    "date_published": "2020-01-01T00:00:00Z",
                },
            ],
        }))
        .unwrap();

        let source = parse(&bytes, &feed(FEED_URL)).unwrap();
        assert!(!source.articles().is_empty());
        for article in source.articles() {
            for out in [article.title(), article.source_title()] {
                for c in ['<', '>', '"', '\''] {
                    assert!(!out.contains(c), "{c:?} in sanitized text: {out:?}");
                }
            }
        }
    }

    #[test]
    fn parse_sanitizes_title_and_source_title() {
        // The XML parser decodes the entities, so the parsed titles hold
        // real <script> markup, just as a hostile Feed would deliver them.
        let source = parse_xml(
            r#"<?xml version="1.0" encoding="utf-8"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>&lt;script&gt;evil()&lt;/script&gt;Src</title>
                <link href="https://example.com/"/>
                <updated>2020-01-01T00:00:00Z</updated>
                <entry>
                    <title>&lt;script&gt;alert(1)&lt;/script&gt;Hello</title>
                    <link href="https://example.com/x"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>s</summary>
                </entry>
            </feed>"#,
        );
        let [article] = source.articles() else {
            panic!("expected one article: {:?}", source.articles());
        };
        assert_eq!(article.title(), "Hello");
        assert_eq!(article.source_title(), "Src");
    }

    // A summary with nothing safe left after sanitizing counts as missing,
    // so the Article's page gets a chance to supply one.
    #[test]
    fn a_summary_with_nothing_safe_left_is_missing() {
        let source = parse_xml(&atom(
            r#"<entry>
                <title>Only script</title>
                <link href="https://example.com/first"/>
                <published>2020-01-01T00:00:00Z</published>
                <summary>&lt;script&gt;alert(1)&lt;/script&gt;</summary>
            </entry>"#,
        ));
        assert_eq!(source.articles()[0].summary(), None);
    }

    #[test]
    fn parse_warns_once_when_a_feed_yields_nothing() {
        let xml = atom(
            r"<entry>
                <title>No Link A</title>
                <published>2021-01-01T00:00:00Z</published>
            </entry>
            <entry>
                <title>No Link B</title>
                <published>2021-01-02T00:00:00Z</published>
            </entry>",
        );

        let logs = warnings_logged(|| {
            assert!(parse_xml(&xml).articles().is_empty());
        });
        // One aggregated warning naming the Feed and the count, not one line
        // per malformed Entry.
        assert_eq!(
            logs.matches("contributed no articles").count(),
            1,
            "logs: {logs}"
        );
        assert!(logs.contains("skipped=2"), "logs: {logs}");
    }

    #[test]
    fn parse_is_quiet_about_partial_skips() {
        let xml = atom(
            r#"<entry>
                <title>No Link</title>
                <published>2021-01-01T00:00:00Z</published>
            </entry>
            <entry>
                <title>Good</title>
                <link href="https://example.com/g"/>
                <published>2020-01-01T00:00:00Z</published>
                <summary>x</summary>
            </entry>"#,
        );

        let logs = warnings_logged(|| {
            let source = parse_xml(&xml);
            let [article] = source.articles() else {
                panic!("expected one article: {:?}", source.articles());
            };
            assert_eq!(article.title(), "Good");
        });
        // A Feed that still renders something does not warn; per-Entry
        // detail stays at debug level.
        assert_eq!(logs, "", "expected no warnings for a partially usable feed");
    }

    #[test]
    fn parse_skips_and_counts_an_entry_with_an_out_of_range_date() {
        // Year 10000 parses as a Feed date but lies past jiff's range, which
        // ends at year 9999. The RSS date parser accepts a five-digit year.
        let xml = r#"<?xml version="1.0"?>
            <rss version="2.0">
                <channel>
                    <title>t</title>
                    <link>https://example.com/</link>
                    <item>
                        <title>Far Future</title>
                        <link>https://example.com/future</link>
                        <pubDate>01 Jan 10000 00:00:00 GMT</pubDate>
                    </item>
                </channel>
            </rss>"#;

        let logs = warnings_logged(|| {
            assert!(parse_xml(xml).articles().is_empty());
        });
        assert!(logs.contains("skipped=1"), "logs: {logs}");
    }

    // Every complete Entry yields one Article, and an Entry missing its
    // link, title, or date yields none.
    #[hegel::test]
    fn parse_keeps_exactly_the_complete_entries(tc: hegel::TestCase) {
        use std::fmt::Write as _;

        let n = tc.draw(generators::integers::<usize>().min_value(0).max_value(8));
        let mut entries = String::new();
        let mut complete = Vec::new();
        for i in 0..n {
            let has_link = tc.draw(generators::booleans());
            let has_title = tc.draw(generators::booleans());
            let has_date = tc.draw(generators::booleans());
            let mut xml = String::from("<entry>");
            if has_link {
                write!(xml, r#"<link href="/post/{i}"/>"#).unwrap();
            }
            if has_title {
                write!(xml, "<title>{i}</title>").unwrap();
            }
            if has_date {
                xml.push_str("<published>2020-01-01T00:00:00Z</published>");
            }
            xml.push_str("</entry>");
            entries.push_str(&xml);
            if has_link && has_title && has_date {
                complete.push(format!("https://example.com/post/{i}"));
            }
        }

        let source = parse_xml(&atom(&entries));
        let mut got: Vec<String> = source
            .articles()
            .iter()
            .map(|article| article.link().to_string())
            .collect();
        got.sort_unstable();
        complete.sort_unstable();
        assert_eq!(got, complete);
    }

    // Relative Entry hrefs resolve against the Feed URL, so every Article
    // link is absolute with the Feed's scheme and a host.
    #[hegel::test]
    fn parse_article_links_are_absolute(tc: hegel::TestCase) {
        let host = tc.draw(generators::domains());
        let feed_str = format!("https://{host}/blog/feed.xml");
        let href = tc.draw(hegel::one_of!(
            generators::just("/post".to_string()),
            generators::just("post".to_string()),
            generators::just("../post?x=1".to_string()),
            generators::just("//other.example/post".to_string()),
            generators::just("https://elsewhere.example/post".to_string()),
            generators::from_regex(r"[a-z0-9/]{0,12}").fullmatch(true),
        ));

        let source = parse_at(
            &feed_str,
            &atom(&format!(
                r#"<entry>
                    <title>t</title>
                    <link href="{href}"/>
                    <published>2020-01-01T00:00:00Z</published>
                </entry>"#
            )),
        );
        for article in source.articles() {
            assert_eq!(article.link().scheme(), "https");
            assert!(article.link().host_str().is_some(), "{}", article.link());
        }
    }

    // Articles come out sorted, so selection can take a prefix.
    #[hegel::test]
    fn parse_sorts_articles_by_article_order(tc: hegel::TestCase) {
        use std::fmt::Write as _;

        let days = tc.draw(generators::vecs(generators::integers::<u8>().max_value(3)).max_size(8));
        let mut entries = String::new();
        for (i, day) in days.iter().enumerate() {
            write!(
                entries,
                r#"<entry>
                    <title>{i}</title>
                    <link href="/{i}"/>
                    <published>2020-01-0{}T00:00:00Z</published>
                </entry>"#,
                day + 1
            )
            .unwrap();
        }
        let source = parse_xml(&atom(&entries));
        assert!(
            source
                .articles()
                .windows(2)
                .all(|w| article_order(&w[0], &w[1]).is_le())
        );
    }
}
