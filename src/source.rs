//! Projection of a fetched document onto its Source and Articles.
//!
//! Nothing in a fetched document can fail the run. A bad link falls back,
//! and an Entry that cannot become an Article is skipped and counted.

use feed_rs::model::{Entry, Feed as Document, Link};
use jiff::Timestamp;
use tracing::{debug, info, warn};
use url::Url;

use crate::{Article, feed_list::Feed};

/// A fetched Feed with every complete Entry as an Article. Each Article
/// carries the Source's display title and homepage link.
#[derive(Debug)]
pub(crate) struct Source {
    pub(crate) feed: Feed,
    /// Newest first by [`article_order`].
    pub(crate) articles: Vec<Article>,
}

/// Project a fetched `document` for `feed` onto its Source.
///
/// Warns once when the document lists Entries but none of them is complete,
/// so a feed that yields nothing usable does not vanish silently.
pub(crate) fn project(document: &Document, feed: Feed) -> Source {
    let title = sanitize_text(&resolve_source_title(document, &feed.url));
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

/// The href of the first link explicitly tagged `rel="alternate"`, if any.
fn find_alternate_link(links: &[Link]) -> Option<&str> {
    links
        .iter()
        .find(|l| l.rel.as_deref() == Some("alternate"))
        .map(|l| l.href.as_str())
}

/// The display title for a feed: its declared title, or the fetch URL's host
/// (domain or IP address) when the feed omits the title or leaves it blank.
fn resolve_source_title(document: &Document, feed_url: &Url) -> String {
    match &document.title {
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
/// An href that does not resolve against the feed URL is skipped, and the
/// next link in that order applies.
fn resolve_source_link(document: &Document, feed_url: &Url) -> Url {
    let src = document.title.as_ref().and_then(|t| t.src.as_deref());
    let alternate = find_alternate_link(&document.links);
    // Ignore "self" rels, which usually link back to the feed itself.
    let others = document
        .links
        .iter()
        .filter(|l| l.rel.as_deref() != Some("self"))
        .map(|l| l.href.as_str());
    if let Some(url) = src
        .into_iter()
        .chain(alternate)
        .chain(others)
        .find_map(|href| feed_url.join(href).ok())
    {
        return url;
    }
    warn!(
        source = feed_url.as_str(),
        "feed is missing root link: falling back to rss feed url."
    );
    feed_url.clone()
}

/// The best link for an entry, resolved against the feed URL.
///
/// Prefers an `alternate` link, falling back to the first link present.
/// Comment links are never chosen: they point at discussion of the post, not
/// the post itself. `None` when the entry has no other links or the href
/// cannot be parsed; the caller skips such entries like any other incomplete
/// entry.
fn resolve_entry_link(entry: &Entry, feed_url: &Url) -> Option<Url> {
    let href = find_alternate_link(&entry.links).or_else(|| {
        entry
            .links
            .iter()
            .find(|link| link.target.is_none())
            .map(|link| link.href.as_str())
    })?;
    feed_url.join(href).ok()
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

/// Build a renderable [`Article`] from a feed entry. `source_title` must
/// already be sanitized.
///
/// Returns `None` exactly when the entry is incomplete: missing a usable
/// link, title, or date, or carrying a date outside the representable
/// range. Selection policy (date cutoff, per-source caps) belongs to
/// [`crate::select`].
fn build_article(
    entry: &Entry,
    feed_url: &Url,
    source_title: &str,
    source_link: &Url,
) -> Option<Article> {
    let (Some(link), Some(title), Some(timestamp)) = (
        resolve_entry_link(entry, feed_url),
        entry.title.as_ref().map(|t| &t.content),
        entry
            .published
            .or(entry.updated)
            .and_then(|date| Timestamp::from_second(date.timestamp()).ok()),
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

    let summary = raw_summary(entry).map_or_else(
        || {
            info!(?link, ?source_link, "no summary or content provided.");
            String::new()
        },
        sanitize_html,
    );

    Some(Article {
        link,
        // Titles are feed-controlled and the default template embeds them
        // with `| safe`, so they get the same boundary treatment as summaries.
        title: sanitize_text(title),
        summary,
        source_link: source_link.clone(),
        source_title: source_title.to_owned(),
        timestamp,
    })
}

#[cfg(test)]
mod tests {
    use feed_rs::model::{Entry, Feed as Document, Link};
    use hegel::generators;
    use url::Url;

    use super::{
        Source, article_order, build_article, find_alternate_link, project, raw_summary,
        resolve_entry_link, resolve_source_link, resolve_source_title, sanitize_html,
    };
    use crate::{feed_list::Feed, test_logs::warnings_logged};

    const FEED_URL: &str = "https://example.com/feed.xml";

    fn feed_url() -> Url {
        Url::parse(FEED_URL).unwrap()
    }

    // Parse a feed fixture the same way production does.
    fn parse_feed(xml: &str) -> Document {
        feed_rs::parser::parse(xml.as_bytes()).unwrap()
    }

    // Project `xml` as if it were fetched from the unweighted Feed `url`.
    fn project_at(url: &str, xml: &str) -> Source {
        let feed = Feed {
            url: Url::parse(url).unwrap(),
            weight: None,
        };
        project(&parse_feed(xml), feed)
    }

    fn project_xml(xml: &str) -> Source {
        project_at(FEED_URL, xml)
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
            target: None,
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
            generators::ip_addresses().v4().map(|ip| ip.to_string()),
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
    fn project_source_link_prefers_alternate_then_other_then_feed_url() {
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
            resolve_source_link(&alt, &feed_url()).as_str(),
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
            resolve_source_link(&other, &feed_url()).as_str(),
            "https://example.com/related"
        );
    }

    #[test]
    fn project_source_link_falls_back_to_feed_url_without_usable_links() {
        // A titleless feed whose only link is `self` has nothing else to point
        // at, so the feed URL itself is used. This also guards against a
        // panic on a missing <title>.
        let bare = parse_feed(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <link rel="self" href="https://example.com/feed.xml"/>
                <updated>2020-01-01T00:00:00Z</updated>
            </feed>"#,
        );
        assert_eq!(resolve_source_link(&bare, &feed_url()), feed_url());
    }

    #[test]
    fn project_accepts_an_unparseable_feed_level_href() {
        // "http://[" is not a valid URL and joins to nothing. The source
        // link falls back to the feed URL, and the run goes on.
        let source = project_xml(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>t</title>
                <link rel="alternate" href="http://["/>
                <updated>2020-01-01T00:00:00Z</updated>
                <entry>
                    <title>Good</title>
                    <link href="https://example.com/good"/>
                    <published>2020-01-01T00:00:00Z</published>
                </entry>
            </feed>"#,
        );
        assert_eq!(source.articles[0].source_link, feed_url());
        assert_eq!(source.articles.len(), 1);
    }

    // Unparseable hrefs at any step of the source-link chain never stop
    // projection. The first href that resolves wins, and the feed URL is
    // the last resort.
    #[hegel::test]
    fn project_source_link_skips_unparseable_hrefs(tc: hegel::TestCase) {
        // Each href is either valid or one that `Url::join` rejects.
        let pick = |good: &'static str| {
            hegel::one_of!(
                generators::just(Some(good)),
                generators::just(Some("http://[")),
                generators::just(None::<&'static str>),
            )
        };
        let src = tc.draw(pick("https://example.com/src"));
        let alternate = tc.draw(pick("https://example.com/alt"));
        let other = tc.draw(pick("https://example.com/other"));

        let mut document = parse_feed(&atom(""));
        document.links.clear();
        if let Some(title) = document.title.as_mut() {
            title.src = src.map(str::to_owned);
        }
        if let Some(href) = alternate {
            document.links.push(link(href, Some("alternate")));
        }
        if let Some(href) = other {
            document.links.push(link(href, Some("related")));
        }
        let good = |href: Option<&'static str>| href.filter(|h| !h.contains('['));
        let expected = good(src)
            .or(good(alternate))
            .or(good(other))
            .map_or_else(feed_url, |h| Url::parse(h).unwrap());
        assert_eq!(resolve_source_link(&document, &feed_url()), expected);
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
    fn resolve_entry_link_ignores_comment_links() {
        // feed-rs lists RSS `<comments>` and `wfw:commentRss` as entry links,
        // in document order, so a comments link can come first. The article
        // must point at the post itself.
        let feed = parse_feed(
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
                    </item>
                    <item>
                        <title>b</title>
                        <comments>https://example.com/b#comments</comments>
                    </item>
                </channel>
            </rss>"#,
        );
        let links: Vec<_> = feed
            .entries
            .iter()
            .map(|entry| resolve_entry_link(entry, &feed_url()).map(String::from))
            .collect();
        assert_eq!(links, [Some("https://example.com/a".to_owned()), None]);
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
    // with a live <script> tag. Decoding entities after cleaning would turn
    // harmless text like "&lt;script&gt;" back into real markup, which the
    // default template then embeds raw via `| safe`.
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
    fn project_sanitizes_title_and_source_title() {
        // The XML parser decodes the entities, so the in-memory titles hold
        // real <script> markup, exactly as a hostile feed would deliver it.
        let source = project_xml(
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
        assert_eq!(source.articles[0].title, "Hello");
        assert_eq!(source.articles[0].source_title, "Src");
    }

    #[test]
    fn build_article_assembles_fields_and_skips_incomplete() {
        let source_link = Url::parse("https://example.com/").unwrap();
        let complete = first_entry(
            r#"<entry>
                <title>Complete Post</title>
                <link href="https://example.com/complete"/>
                <published>2024-01-01T00:00:00Z</published>
                <summary>hello</summary>
            </entry>"#,
        );

        // Every field of a complete entry flows through, source info included.
        // The date cutoff is selection's concern, exercised there.
        let built = build_article(&complete, &feed_url(), "Src", &source_link).unwrap();
        assert_eq!(built.title, "Complete Post");
        assert_eq!(built.link.as_str(), "https://example.com/complete");
        assert_eq!(built.summary, "hello");
        assert_eq!(built.source_title, "Src");
        assert_eq!(built.source_link, source_link);

        // An entry missing its date is skipped rather than erroring.
        let dateless = first_entry(
            r#"<entry>
                <title>Dateless</title>
                <link href="https://example.com/x"/>
                <summary>s</summary>
            </entry>"#,
        );
        assert!(build_article(&dateless, &feed_url(), "Src", &source_link).is_none());
    }

    #[test]
    fn project_extracts_basic_fields() {
        let source = project_xml(&atom(
            r#"<entry>
                <title>First Post</title>
                <link href="https://example.com/first"/>
                <published>2020-01-01T00:00:00Z</published>
                <summary>Hello world</summary>
            </entry>"#,
        ));

        assert_eq!(source.articles.len(), 1);
        let a = &source.articles[0];
        assert_eq!(a.link.as_str(), "https://example.com/first");
        assert_eq!(a.title, "First Post");
        assert_eq!(a.summary, "Hello world");
        assert_eq!(a.source_title, "Example Blog");
        assert_eq!(a.source_link.as_str(), "https://example.com/");
    }

    #[test]
    fn project_orders_articles_newest_first() {
        // Nothing in RSS/Atom promises newest-first, so document order must
        // not matter.
        let source = project_xml(&atom(
            r#"<entry>
                <title>Oldest</title>
                <link href="https://example.com/1"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>
            <entry>
                <title>Newest</title>
                <link href="https://example.com/3"/>
                <published>2022-01-01T00:00:00Z</published>
            </entry>
            <entry>
                <title>Middle</title>
                <link href="https://example.com/2"/>
                <published>2021-01-01T00:00:00Z</published>
            </entry>"#,
        ));
        let titles: Vec<_> = source.articles.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["Newest", "Middle", "Oldest"]);
    }

    #[test]
    fn project_warns_once_when_a_feed_yields_nothing() {
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
            assert!(project_xml(&xml).articles.is_empty());
        });
        // One aggregated warning naming the feed and the count, not one line
        // per malformed entry.
        assert_eq!(
            logs.matches("contributed no articles").count(),
            1,
            "logs: {logs}"
        );
        assert!(logs.contains("skipped=2"), "logs: {logs}");
    }

    #[test]
    fn project_is_quiet_about_partial_skips() {
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
            let source = project_xml(&xml);
            assert_eq!(source.articles.len(), 1);
            assert_eq!(source.articles[0].title, "Good");
        });
        // A feed that still renders something is not warn-worthy; the
        // per-entry detail lives at debug level.
        assert_eq!(logs, "", "expected no warnings for a partially usable feed");
    }

    #[test]
    fn project_skips_and_counts_an_entry_with_an_out_of_range_date() {
        // Year 10000 parses as a feed date but lies past jiff's range, which
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
        assert!(
            parse_feed(xml).entries[0].published.is_some(),
            "fixture must carry a parsed date"
        );

        let logs = warnings_logged(|| {
            assert!(project_xml(xml).articles.is_empty());
        });
        assert!(logs.contains("skipped=1"), "logs: {logs}");
    }

    // Every complete Entry yields exactly one Article, and an Entry missing
    // its link, title, or date yields none.
    #[hegel::test]
    fn project_keeps_exactly_the_complete_entries(tc: hegel::TestCase) {
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

        let source = project_xml(&atom(&entries));
        let mut got: Vec<String> = source.articles.iter().map(|a| a.link.to_string()).collect();
        got.sort_unstable();
        complete.sort_unstable();
        assert_eq!(got, complete);
    }

    // Relative entry hrefs resolve against the feed URL, so every Article
    // link is absolute with the feed's scheme and a host.
    #[hegel::test]
    fn project_article_links_are_absolute(tc: hegel::TestCase) {
        let host = tc.draw(generators::domains());
        let feed = format!("https://{host}/blog/feed.xml");
        let href = tc.draw(hegel::one_of!(
            generators::just("/post".to_string()),
            generators::just("post".to_string()),
            generators::just("../post?x=1".to_string()),
            generators::just("//other.example/post".to_string()),
            generators::just("https://elsewhere.example/post".to_string()),
            generators::from_regex(r"[a-z0-9/]{0,12}").fullmatch(true),
        ));

        let source = project_at(
            &feed,
            &atom(&format!(
                r#"<entry>
                    <title>t</title>
                    <link href="{href}"/>
                    <published>2020-01-01T00:00:00Z</published>
                </entry>"#
            )),
        );
        for article in &source.articles {
            assert_eq!(article.link.scheme(), "https");
            assert!(article.link.host_str().is_some(), "{}", article.link);
        }
    }

    // Articles come out sorted, so selection can take a prefix.
    #[hegel::test]
    fn project_sorts_articles_by_article_order(tc: hegel::TestCase) {
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
        let source = project_xml(&atom(&entries));
        assert!(
            source
                .articles
                .windows(2)
                .all(|w| article_order(&w[0], &w[1]).is_le())
        );
    }
}
