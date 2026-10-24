//! Projection of a fetched document onto its Source and Articles.
//!
//! Nothing in a fetched document can fail the run. A bad link falls back,
//! and an Entry that cannot become an Article is skipped and counted.

use feed_rs::model::{Entry, Feed as Document, Link};
use jiff::Timestamp;
use tracing::{debug, info, warn};
use url::{Position, Url};

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
/// link, a title a reader can see, or a date jiff can represent. Selection
/// policy (date cutoff, per-source caps) belongs to [`crate::select`].
fn build_article(
    entry: &Entry,
    feed_url: &Url,
    source_title: &str,
    source_link: &Url,
) -> Option<Article> {
    let (Some(link), Some(title), Some(timestamp)) = (
        resolve_entry_link(entry, feed_url),
        entry.title.as_ref().and_then(|t| visible_text(&t.content)),
        // The first date jiff can represent, so an out-of-range published
        // date does not hide a usable updated one.
        [entry.published, entry.updated]
            .into_iter()
            .flatten()
            .find_map(|date| Timestamp::from_second(date.timestamp()).ok()),
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
    use feed_rs::model::{Entry, Feed as Document, Link};
    use hegel::generators;
    use url::Url;

    use super::{
        Source, article_order, build_article, project, raw_summary, resolve_entry_link,
        resolve_source_link, resolve_source_title, sanitize_html,
    };
    use crate::{feed_list::Feed, test_support::warnings_logged};

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

    fn entry_with(links: Vec<Link>) -> Entry {
        Entry {
            links,
            ..Entry::default()
        }
    }

    // Whatever links an entry carries, its Article link is a web page that
    // is not the feed itself, and holds nothing that could end a quoted
    // attribute. A feed controls these links and the bundled template
    // embeds them through `| safe`, so any other scheme or a stray quote
    // could run script in the reader's page.
    #[hegel::test]
    fn entry_links_are_only_web_pages_other_than_the_feed(tc: hegel::TestCase) {
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
                generators::text().max_size(24)
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
        let links = (0..n)
            .map(|_| link(&tc.draw(href()), tc.draw(rel())))
            .collect();

        if let Some(url) = resolve_entry_link(&entry_with(links), &feed_url()) {
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
        // The only link runs script, so the entry has no usable link.
        let only_script = entry_with(vec![link(
            "javascript:\"><img src=x onerror=alert(1)>",
            Some("alternate"),
        )]);
        assert_eq!(resolve_entry_link(&only_script, &feed_url()), None);

        // A safe link after it wins instead.
        let with_page = entry_with(vec![
            link("javascript:alert(1)", Some("alternate")),
            link("https://example.com/post", None),
        ]);
        assert_eq!(
            resolve_entry_link(&with_page, &feed_url())
                .unwrap()
                .as_str(),
            "https://example.com/post"
        );
    }

    #[test]
    fn an_entry_link_back_to_the_feed_is_not_a_link_to_the_post() {
        let entry = entry_with(vec![
            link("", Some("alternate")),
            link("#comments", None),
            link("https://example.com/post", None),
        ]);
        assert_eq!(
            resolve_entry_link(&entry, &feed_url()).unwrap().as_str(),
            "https://example.com/post"
        );
    }

    // A feed URL may hold an apostrophe. Links that resolve back to it are
    // still the feed, not the post.
    #[test]
    fn a_link_back_to_a_feed_url_with_an_apostrophe_is_still_the_feed() {
        let feed = Url::parse("https://example.com/it's/feed.xml").unwrap();
        let entry = entry_with(vec![
            link("", Some("alternate")),
            link("#top", None),
            link("feed.xml", None),
            link("post", None),
        ]);
        assert_eq!(
            resolve_entry_link(&entry, &feed).unwrap().as_str(),
            "https://example.com/it%27s/post"
        );

        // With no usable homepage link, the fallback to the feed URL is
        // escaped the same way.
        let mut document = parse_feed(&atom(""));
        document.links.clear();
        assert_eq!(
            resolve_source_link(&document, &feed).as_str(),
            "https://example.com/it%27s/feed.xml"
        );
    }

    #[test]
    fn a_bad_alternate_falls_back_to_the_next_content_link() {
        let entry = entry_with(vec![
            link("http://[", Some("alternate")),
            link("https://example.com/post", None),
        ]);
        assert_eq!(
            resolve_entry_link(&entry, &feed_url()).unwrap().as_str(),
            "https://example.com/post"
        );
    }

    #[test]
    fn atom_comment_edit_and_enclosure_links_are_never_the_post() {
        let entry = entry_with(vec![
            link("https://example.com/post/comments", Some("replies")),
            link("https://example.com/post/edit", Some("edit")),
            link("https://example.com/post.mp3", Some("enclosure")),
        ]);
        assert_eq!(resolve_entry_link(&entry, &feed_url()), None);
    }

    #[test]
    fn the_homepage_is_never_a_hub_or_paging_link() {
        let mut document = parse_feed(&atom(""));
        document.links = vec![
            link("https://hub.example/", Some("hub")),
            link("https://example.com/feed.xml?page=2", Some("next")),
            link("https://example.com/", None),
        ];
        assert_eq!(
            resolve_source_link(&document, &feed_url()).as_str(),
            "https://example.com/"
        );
    }

    // A title a reader could not see falls back to the host for a feed,
    // and makes an entry incomplete.
    #[test]
    fn invisible_titles_count_as_missing() {
        for title in [
            "   ",
            "&lt;b&gt;&lt;/b&gt;",
            "&lt;script&gt;x&lt;/script&gt;",
            "&amp;nbsp;",
        ] {
            let feed = parse_feed(&format!(
                r#"<?xml version="1.0"?>
                <feed xmlns="http://www.w3.org/2005/Atom">
                    <title>{title}</title>
                    <updated>2020-01-01T00:00:00Z</updated>
                    <entry>
                        <title>{title}</title>
                        <link href="https://example.com/post"/>
                        <published>2020-01-01T00:00:00Z</published>
                    </entry>
                </feed>"#
            ));
            assert_eq!(
                resolve_source_title(&feed, &feed_url()),
                "example.com",
                "{title:?}"
            );
            assert!(
                build_article(&feed.entries[0], &feed_url(), "Src", &feed_url()).is_none(),
                "{title:?}"
            );
        }
    }

    #[test]
    fn an_out_of_range_published_date_falls_back_to_updated() {
        // The RSS date parser accepts a five-digit year, past the end of
        // jiff's range in year 9999.
        let far = parse_feed(
            r#"<?xml version="1.0"?>
            <rss version="2.0"><channel><title>t</title><link>https://example.com/</link>
                <item>
                    <title>t</title>
                    <link>https://example.com/post</link>
                    <pubDate>01 Jan 10000 00:00:00 GMT</pubDate>
                </item>
            </channel></rss>"#,
        );
        let mut entry = far.entries[0].clone();
        assert!(
            entry.published.is_some(),
            "fixture must carry a parsed date"
        );
        entry.updated = Some("2020-01-01T00:00:00Z".parse().unwrap());

        let source_link = Url::parse("https://example.com/").unwrap();
        let article = build_article(&entry, &feed_url(), "Src", &source_link);
        assert_eq!(
            article.map(|a| a.timestamp.to_string()).as_deref(),
            Some("2020-01-01T00:00:00Z")
        );
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
    fn project_source_link_prefers_alternate_then_plain_then_feed_url() {
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

        // An RSS channel's plain <link> has no rel, and it is the homepage
        // even when a self link comes first.
        let plain = parse_feed(
            r#"<?xml version="1.0"?>
            <rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom">
                <channel>
                    <title>t</title>
                    <atom:link rel="self" href="https://example.com/feed.xml"/>
                    <link>https://example.com/plain</link>
                    <description>d</description>
                </channel>
            </rss>"#,
        );
        assert_eq!(
            resolve_source_link(&plain, &feed_url()).as_str(),
            "https://example.com/plain"
        );

        // A related link points somewhere else, so the feed URL stands in.
        let related = parse_feed(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>t</title>
                <link rel="self" href="https://example.com/feed.xml"/>
                <link rel="related" href="https://example.com/related"/>
                <updated>2020-01-01T00:00:00Z</updated>
            </feed>"#,
        );
        assert_eq!(resolve_source_link(&related, &feed_url()), feed_url());
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

    // Unparseable hrefs at any step of the source-link chain never stop
    // projection. The first href that resolves wins, and the feed URL is
    // the last resort.
    #[hegel::test]
    fn project_source_link_skips_unparseable_hrefs(tc: hegel::TestCase) {
        // Each href is valid, unparseable, or not a web page.
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

        let mut document = parse_feed(&atom(""));
        document.links.clear();
        if let Some(title) = document.title.as_mut() {
            title.src = src.map(str::to_owned);
        }
        if let Some(href) = alternate {
            document.links.push(link(href, Some("alternate")));
        }
        if let Some(href) = other {
            document.links.push(link(href, None));
        }
        let good = |href: Option<&'static str>| href.filter(|h| h.starts_with("https://"));
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

    // Sanitized text holds no character that could open a tag or end a
    // quoted attribute, so it is safe anywhere a template puts it.
    #[hegel::test]
    fn sanitize_text_never_emits_markup_or_quotes(tc: hegel::TestCase) {
        let payload = tc.draw(hegel::one_of!(
            generators::just("\"><img src=x onerror=alert(1)>".to_string()),
            generators::just("' onmouseover='alert(1)".to_string()),
            generators::just("&quot; onclick=&quot;alert(1)".to_string()),
            generators::just("&lt;script&gt;alert(1)&lt;/script&gt;".to_string()),
            generators::text(),
        ));
        let prefix = tc.draw(generators::text().max_size(16));
        let suffix = tc.draw(generators::text().max_size(16));
        let out = super::sanitize_text(&format!("{prefix}{payload}{suffix}"));
        for c in ['<', '>', '"', '\''] {
            assert!(!out.contains(c), "{c:?} in sanitized text: {out:?}");
        }
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
        assert_eq!(a.summary.as_deref(), Some("Hello world"));
        assert_eq!(a.source_title, "Example Blog");
        assert_eq!(a.source_link.as_str(), "https://example.com/");
    }

    // A summary with nothing safe left after sanitizing counts as missing,
    // so the Article's page gets a chance to supply one.
    #[test]
    fn a_summary_with_nothing_safe_left_is_missing() {
        let source = project_xml(&atom(
            r#"<entry>
                <title>Only script</title>
                <link href="https://example.com/first"/>
                <published>2020-01-01T00:00:00Z</published>
                <summary>&lt;script&gt;alert(1)&lt;/script&gt;</summary>
            </entry>"#,
        ));
        assert_eq!(source.articles[0].summary, None);
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
