//! Derive summaries for the Articles that will render but whose feed gave
//! none, by fetching each Article's own page and reading the page's
//! description of itself.

use scraper::{Html, Selector};
use tracing::debug;
use url::Url;

use crate::{
    fan_out::{Outcome, Words, fan_out},
    http::Http,
    source::Article,
    terminal::Terminal,
};

/// The largest page body worth downloading for a summary. Article pages run
/// well under 1 MiB of HTML; anything bigger is almost certainly media or a
/// mislabeled download, and only the head of the page matters here anyway.
const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;

/// Cap on a derived summary, in characters. Roughly two or three sentences:
/// enough to say what the article is about, short enough that a webring
/// entry stays a teaser rather than a reprint.
const MAX_SUMMARY_CHARS: usize = 500;

/// The words the summary fan-out shows. A page that yields nothing is
/// skipped without a report, so the stale and failed words are never used.
const SUMMARY_WORDS: Words = Words {
    progress: "Summarizing",
    done: "Summary",
    stale_log: "using a fallback summary",
    failed_log: "could not derive a summary",
    panic_log: "summary fetch task failed",
};

/// Give each of `articles` that has no summary one derived from its own
/// page, through the shared `http` client, with progress on `terminal`.
///
/// Pass only the Articles that will render: each missing summary costs a
/// page fetch. An Article whose page yields nothing keeps no summary, and
/// nothing here can fail the run.
pub(crate) async fn fill_missing_summaries(
    http: &Http,
    terminal: &Terminal,
    articles: &mut [Article],
) {
    let items = articles
        .iter()
        .enumerate()
        .filter(|(_, a)| !a.has_summary())
        .map(|(i, a)| (a.link().clone(), i))
        .collect();
    let summaries = fan_out(terminal, &SUMMARY_WORDS, items, |url, i| {
        let http = http.clone();
        async move {
            // A page with nothing quotable is routine, not worth a report.
            fetch_summary(&http, &url)
                .await
                .map_or(Outcome::Skipped, |summary| Outcome::Done((i, summary)))
        }
    })
    .await;
    for (i, summary) in summaries {
        articles[i].fill_summary(&summary);
    }
}

/// Fetch `url` and derive a summary from the page itself.
///
/// `None` simply means no summary could be derived (network failure, a
/// non-HTML link, or a page with nothing quotable), which the caller renders
/// exactly like a feed that offered no summary, so nothing here can fail the
/// run. The returned text is plain text: [`Article::fill_summary`] escapes
/// it.
async fn fetch_summary(http: &Http, url: &Url) -> Option<String> {
    let resp = match http.get(url, &[]).await {
        Ok(resp) => resp,
        Err(e) => {
            // Only worth a debug line: the article renders fine without a
            // summary, and the feed's own fetch already surfaces dead hosts.
            debug!(url = url.as_str(), error = %e, "could not fetch page to derive a summary");
            return None;
        }
    };
    let status = resp.status();
    if status.is_client_error() || status.is_server_error() {
        debug!(url = url.as_str(), %status, "page fetch failed; no summary to derive");
        return None;
    }

    let content_type = resp.header("content-type");
    if !is_html(content_type) {
        debug!(
            url = url.as_str(),
            content_type, "not an HTML page; no summary to derive"
        );
        return None;
    }

    let body = match resp.read_capped(MAX_PAGE_BYTES).await {
        Ok(body) => body,
        Err(e) => {
            debug!(url = url.as_str(), error = %e, "could not read the page; no summary derived");
            return None;
        }
    };
    // Non-UTF-8 pages are rare enough that lossy decoding is fine: mangled
    // bytes degrade a summary, not the run.
    extract_summary(&String::from_utf8_lossy(&body))
}

/// Whether a Content-Type header names something the HTML extractor can read.
///
/// An absent header gets the benefit of the doubt: parsing non-HTML text
/// finds no description and no paragraphs, which is a clean `None` anyway.
fn is_html(content_type: Option<&str>) -> bool {
    let Some(value) = content_type else {
        return true;
    };
    let mime = value.split(';').next().unwrap_or("").trim();
    mime.eq_ignore_ascii_case("text/html") || mime.eq_ignore_ascii_case("application/xhtml+xml")
}

/// Derive a plain-text summary from an HTML page: the page's own meta
/// description when it carries one, otherwise its leading paragraphs.
///
/// Output is whitespace-normalized plain text, cut to [`MAX_SUMMARY_CHARS`].
/// It is not escaped for HTML.
fn extract_summary(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let text = meta_description(&doc).or_else(|| leading_paragraphs(&doc))?;
    Some(truncate_chars(text, MAX_SUMMARY_CHARS))
}

/// The page's own summary of itself, from standard description metadata.
/// Plain `description` first: it is written as prose, while the social-card
/// variants are sometimes stuffed with taglines.
fn meta_description(doc: &Html) -> Option<String> {
    const SELECTORS: [&str; 3] = [
        r#"meta[name="description"]"#,
        r#"meta[property="og:description"]"#,
        r#"meta[name="twitter:description"]"#,
    ];
    SELECTORS.iter().find_map(|raw| {
        let selector = Selector::parse(raw).expect("selector literal is valid");
        doc.select(&selector)
            .find_map(|el| el.value().attr("content"))
            .map(|content| normalized([content]))
            .filter(|text| !text.is_empty())
    })
}

/// The text of the page's leading paragraphs, scoped to the most specific
/// content landmark the page has. Scoping to `<article>` or `<main>` first
/// keeps navigation, sidebars, and footers out of the summary on pages with
/// semantic markup; bare `body p` is the last resort for pages without it.
fn leading_paragraphs(doc: &Html) -> Option<String> {
    for scope in ["article p", "main p", "body p"] {
        let selector = Selector::parse(scope).expect("selector literal is valid");
        let mut text = String::new();
        for paragraph in doc.select(&selector) {
            let para = normalized(paragraph.text());
            if para.is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&para);
            // Enough to fill the cap; anything further would be cut anyway.
            if text.chars().count() >= MAX_SUMMARY_CHARS {
                break;
            }
        }
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

/// `parts` joined into one line: runs of whitespace collapse to single
/// spaces and the ends are trimmed, so multi-node HTML text and sloppy
/// attribute values read as prose.
fn normalized<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for token in parts.into_iter().flat_map(str::split_whitespace) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(token);
    }
    out
}

/// `text` cut to at most `max` characters, ellipsis included. The cut lands
/// at the last word boundary that fits, so the ellipsis never splits a word;
/// text already within the cap comes back untouched, which also makes the
/// cut idempotent.
fn truncate_chars(text: String, max: usize) -> String {
    // Byte offset where character number `max` starts; `None` means the
    // text already fits.
    let Some((overflow, _)) = text.char_indices().nth(max) else {
        return text;
    };
    // Keep at most `max - 1` characters of text so the ellipsis stays
    // within the cap.
    let room = text
        .char_indices()
        .nth(max.saturating_sub(1))
        .map_or(overflow, |(i, _)| i);
    // A head with no whitespace is one overlong word; cutting mid-word is
    // the only option left.
    let cut = text[..room].rfind(char::is_whitespace).unwrap_or(room);
    let mut out = text[..cut].trim_end().to_string();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use hegel::generators;
    use url::Url;

    use super::{
        MAX_PAGE_BYTES, MAX_SUMMARY_CHARS, extract_summary, fill_missing_summaries, normalized,
        truncate_chars,
    };
    use crate::{
        http::{Answer, Http},
        source::Article,
        terminal::{Line, Terminal, Tone},
        test_support::block_on,
    };

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn post_url() -> Url {
        url("https://blog.example/post")
    }

    fn article_at(link: Url) -> Article {
        Article::fixture(
            &url("https://blog.example/"),
            "Post",
            link,
            jiff::Timestamp::UNIX_EPOCH,
        )
    }

    fn summary_line(link: &Url) -> Line {
        Line {
            word: "Summary",
            tone: Tone::Good,
            detail: link.to_string(),
        }
    }

    struct SummaryOutcome {
        summary: Option<String>,
        lines: Vec<Line>,
    }

    async fn summarize_one(answer: Answer) -> SummaryOutcome {
        let post = post_url();
        let mut articles = [article_at(post.clone())];
        let http = Http::create_null([(post, answer)]);
        let terminal = Terminal::create_null(true);
        let lines = terminal.track_lines();

        fill_missing_summaries(&http, &terminal, &mut articles).await;

        SummaryOutcome {
            summary: articles[0].summary().map(str::to_owned),
            lines: lines.data(),
        }
    }

    async fn summarize_html(html: impl Into<Vec<u8>>) -> SummaryOutcome {
        summarize_one(Answer::ok(html).header("content-type", "text/html; charset=utf-8")).await
    }

    #[tokio::test]
    async fn skips_articles_that_already_have_a_summary() {
        let existing_url = url("https://blog.example/existing");
        let first_missing_url = url("https://blog.example/first");
        let empty_url = url("https://blog.example/empty");
        let second_missing_url = url("https://blog.example/second");

        let mut existing = article_at(existing_url.clone());
        existing.fill_summary("Already present.");
        let mut articles = [
            existing,
            article_at(first_missing_url.clone()),
            article_at(empty_url.clone()),
            article_at(second_missing_url.clone()),
        ];

        let http = Http::create_null([
            (
                existing_url,
                Answer::ok(
                    r#"<html><head><meta name="description" content="Should not overwrite."></head></html>"#,
                )
                .header("content-type", "text/html"),
            ),
            (
                first_missing_url.clone(),
                Answer::ok(
                    r#"<html><head><meta name="description" content="First derived."></head></html>"#,
                )
                .header("content-type", "text/html"),
            ),
            (
                empty_url.clone(),
                Answer::ok("<html><body></body></html>").header("content-type", "text/html"),
            ),
            (
                second_missing_url.clone(),
                Answer::ok("<html><body><p>Second derived.</p></body></html>")
                    .header("content-type", "text/html"),
            ),
        ]);
        let requests = http.track_requests();
        let terminal = Terminal::create_null(true);
        let lines = terminal.track_lines();

        fill_missing_summaries(&http, &terminal, &mut articles).await;

        assert_eq!(articles[0].summary(), Some("Already present."));
        assert_eq!(articles[1].summary(), Some("First derived."));
        assert_eq!(articles[2].summary(), None);
        assert_eq!(articles[3].summary(), Some("Second derived."));

        let mut requested: Vec<Url> = requests.data().into_iter().map(|r| r.url).collect();
        requested.sort();
        assert_eq!(
            requested,
            [
                empty_url,
                first_missing_url.clone(),
                second_missing_url.clone(),
            ]
        );

        let mut shown = lines.data();
        shown.sort_by(|a, b| a.detail.cmp(&b.detail));
        assert_eq!(
            shown,
            [
                summary_line(&first_missing_url),
                summary_line(&second_missing_url),
            ]
        );
    }

    #[tokio::test]
    async fn sends_no_requests_when_every_article_has_a_summary() {
        let mut first = article_at(url("https://blog.example/one"));
        first.fill_summary("One.");
        let mut second = article_at(url("https://blog.example/two"));
        second.fill_summary("Two.");
        let mut articles = [first, second];

        let http = Http::create_null::<Answer>([]);
        let requests = http.track_requests();
        let terminal = Terminal::create_null(true);
        let lines = terminal.track_lines();

        fill_missing_summaries(&http, &terminal, &mut articles).await;

        assert_eq!(articles[0].summary(), Some("One."));
        assert_eq!(articles[1].summary(), Some("Two."));
        assert!(requests.data().is_empty(), "{:?}", requests.data());
        assert!(lines.data().is_empty(), "{:?}", lines.data());
    }

    #[tokio::test]
    async fn html_escapes_derived_summaries_on_the_article() {
        let meta = summarize_html(
            r#"<html><head>
                <meta name="description" content="Cats &amp; dogs &lt;b&gt;bold&lt;/b&gt;">
                </head></html>"#,
        )
        .await;
        assert_eq!(
            meta.summary.as_deref(),
            Some("Cats &amp; dogs &lt;b&gt;bold&lt;/b&gt;")
        );
        assert_eq!(meta.lines, [summary_line(&post_url())]);

        let para = summarize_html(
            "<html><body><article><p>Use &lt;script&gt;alert(1)&lt;/script&gt; &amp; care.</p></article></body></html>",
        )
        .await;
        assert_eq!(
            para.summary.as_deref(),
            Some("Use &lt;script&gt;alert(1)&lt;/script&gt; &amp; care.")
        );
        assert_eq!(para.lines, [summary_line(&post_url())]);
    }

    #[tokio::test]
    async fn meta_description_is_preferred_over_body_paragraphs() {
        let out = summarize_html(
            r#"<html><head>
                <meta name="description" content="Meta wins">
                </head><body><article><p>Body paragraph</p></article></body></html>"#,
        )
        .await;
        assert_eq!(out.summary.as_deref(), Some("Meta wins"));
        assert_eq!(out.lines, [summary_line(&post_url())]);
    }

    #[tokio::test]
    async fn standard_description_beats_social_variants() {
        let out = summarize_html(
            r#"<html><head>
                <meta property="og:description" content="OG desc">
                <meta name="twitter:description" content="Tweet desc">
                <meta name="description" content="Standard desc">
                </head><body></body></html>"#,
        )
        .await;
        assert_eq!(out.summary.as_deref(), Some("Standard desc"));
        assert_eq!(out.lines, [summary_line(&post_url())]);
    }

    #[tokio::test]
    async fn og_description_used_when_no_standard_description() {
        let out = summarize_html(
            r#"<html><head>
                <meta name="twitter:description" content="Tweet desc">
                <meta property="og:description" content="OG desc">
                </head><body></body></html>"#,
        )
        .await;
        assert_eq!(out.summary.as_deref(), Some("OG desc"));
    }

    #[tokio::test]
    async fn twitter_description_used_as_last_meta_resort() {
        let out = summarize_html(
            r#"<html><head>
                <meta name="twitter:description" content="Tweet desc">
                </head><body></body></html>"#,
        )
        .await;
        assert_eq!(out.summary.as_deref(), Some("Tweet desc"));
    }

    #[tokio::test]
    async fn whitespace_only_meta_falls_back_to_paragraphs() {
        let out = summarize_html(
            r#"<html><head>
                <meta name="description" content="   ">
                </head><body><p>Fallback body.</p></body></html>"#,
        )
        .await;
        assert_eq!(out.summary.as_deref(), Some("Fallback body."));
    }

    #[tokio::test]
    async fn leading_paragraphs_used_when_no_meta_description() {
        let out = summarize_html(
            r"<html><body><article>
                <p>First   line.</p>
                <p>Second line.</p>
                </article></body></html>",
        )
        .await;
        assert_eq!(out.summary.as_deref(), Some("First line. Second line."));
        assert_eq!(out.lines, [summary_line(&post_url())]);
    }

    #[tokio::test]
    async fn article_scope_keeps_navigation_out_of_the_summary() {
        let out = summarize_html(
            r"<html><body>
                <nav><p>Home About Contact</p></nav>
                <article><p>The actual article body.</p></article>
                </body></html>",
        )
        .await;
        assert_eq!(out.summary.as_deref(), Some("The actual article body."));
    }

    #[tokio::test]
    async fn main_scope_keeps_navigation_out_when_no_article_exists() {
        let out = summarize_html(
            r"<html><body>
                <nav><p>Home About Contact</p></nav>
                <main><p>The main content.</p></main>
                </body></html>",
        )
        .await;
        assert_eq!(out.summary.as_deref(), Some("The main content."));
    }

    #[tokio::test]
    async fn script_bodies_do_not_leak_into_the_summary() {
        let out = summarize_html(
            r"<html><body>
                <script>var tracking = 1;</script>
                <p>Readable text.</p>
                </body></html>",
        )
        .await;
        assert_eq!(out.summary.as_deref(), Some("Readable text."));
    }

    #[tokio::test]
    async fn empty_paragraphs_are_skipped() {
        let out = summarize_html(
            r"<html><body><article>
                <p></p>
                <p>   </p>
                <p>Real content.</p>
                </article></body></html>",
        )
        .await;
        assert_eq!(out.summary.as_deref(), Some("Real content."));
    }

    #[tokio::test]
    async fn long_summaries_are_truncated_with_an_ellipsis() {
        // 1000 characters, well over the cap.
        let long = "word ".repeat(200);
        let html = format!(
            r#"<html><head><meta name="description" content="{long}"></head><body></body></html>"#
        );
        let out = summarize_html(html).await;
        let summary = out.summary.expect("meta description present");
        assert!(summary.chars().count() <= MAX_SUMMARY_CHARS);
        assert!(summary.ends_with('…'));
    }

    #[tokio::test]
    async fn paragraph_accumulation_stops_at_the_cap() {
        // Each paragraph has 81 characters. Twenty paragraphs exceed the cap,
        // so accumulation stops early and truncates the result.
        let para = format!("<p>{}</p>", "lorem ipsum dolor sit amet ".repeat(3));
        let body = para.repeat(20);
        let html = format!("<html><body><article>{body}</article></body></html>");
        let out = summarize_html(html).await;
        let summary = out.summary.expect("paragraphs present");
        assert!(summary.chars().count() <= MAX_SUMMARY_CHARS);
        assert!(summary.ends_with('…'));
    }

    #[tokio::test]
    async fn empty_page_yields_no_summary() {
        let out = summarize_html("<html><head></head><body></body></html>").await;
        assert_eq!(out.summary, None);
        assert!(out.lines.is_empty(), "{:?}", out.lines);
    }

    #[tokio::test]
    async fn accepts_html_and_absent_content_types() {
        let page =
            r#"<html><head><meta name="description" content="About this post."></head></html>"#;
        for content_type in [
            None,
            Some("text/html"),
            Some("text/html; charset=utf-8"),
            Some("application/xhtml+xml"),
            Some("TEXT/HTML"),
        ] {
            let answer = match content_type {
                Some(value) => Answer::ok(page).header("content-type", value),
                None => Answer::ok(page),
            };
            let out = summarize_one(answer).await;
            assert_eq!(
                out.summary.as_deref(),
                Some("About this post."),
                "{content_type:?}"
            );
            assert_eq!(out.lines, [summary_line(&post_url())], "{content_type:?}");
        }
    }

    #[tokio::test]
    async fn non_html_pages_yield_no_summary() {
        // Carry a valid HTML description so only Content-Type rejects the page.
        let page =
            r#"<html><head><meta name="description" content="Should be ignored."></head></html>"#;
        for content_type in ["application/json", "image/png", "application/rss+xml"] {
            let out = summarize_one(Answer::ok(page).header("content-type", content_type)).await;
            assert_eq!(out.summary, None, "{content_type}");
            assert!(out.lines.is_empty(), "{content_type}: {:?}", out.lines);
        }
    }

    #[tokio::test]
    async fn chunked_oversized_pages_yield_no_summary() {
        let page = format!(
            r#"<html><head><meta name="description" content="Huge."></head><body>{}</body></html>"#,
            "x".repeat(MAX_PAGE_BYTES)
        );
        let out = summarize_one(
            Answer::ok(page)
                .header("content-type", "text/html")
                .chunked(),
        )
        .await;
        assert_eq!(out.summary, None);
        assert!(out.lines.is_empty(), "{:?}", out.lines);
    }

    #[tokio::test]
    async fn pages_announcing_an_oversized_content_length_yield_no_summary() {
        let page = r#"<html><head><meta name="description" content="Huge."></head></html>"#;
        let announced = u64::try_from(MAX_PAGE_BYTES).unwrap() + 1;
        let out = summarize_one(
            Answer::ok(page)
                .header("content-type", "text/html; charset=utf-8")
                .content_length(announced),
        )
        .await;
        assert_eq!(out.summary, None);
        assert!(out.lines.is_empty(), "{:?}", out.lines);
    }

    #[tokio::test]
    async fn unreachable_pages_yield_no_summary() {
        let out = summarize_one(Answer::send_error("connection refused")).await;
        assert_eq!(out.summary, None);
        assert!(out.lines.is_empty(), "{:?}", out.lines);
    }

    // A page that answers with an error status yields no summary and prints no
    // status line, even when the error body holds a description.
    #[hegel::test]
    fn error_statuses_yield_no_summary(tc: hegel::TestCase) {
        let status = tc.draw(generators::integers::<u16>().min_value(400).max_value(599));
        let page = r#"<html><head><meta name="description" content="Not Found"></head></html>"#;
        let out = block_on(summarize_one(
            Answer::status(status)
                .header("content-type", "text/html")
                .body(page),
        ));
        assert_eq!(out.summary, None);
        assert!(out.lines.is_empty(), "{:?}", out.lines);
    }

    // Whatever the page holds, deriving a summary yields text or nothing and
    // never panics.
    #[hegel::test]
    fn extract_summary_never_panics(tc: hegel::TestCase) {
        let html = tc.draw(generators::text());
        let _ = extract_summary(&html);
    }

    // A truncated summary stays within the cap, keeps short text as is, and
    // ends with an ellipsis when cut.
    #[hegel::test]
    fn truncate_chars_respects_the_cap(tc: hegel::TestCase) {
        let text = tc.draw(generators::text());
        let max = tc.draw(generators::integers::<usize>().min_value(1).max_value(1000));
        let out = truncate_chars(text.clone(), max);
        assert!(out.chars().count() <= max);
        if text.chars().count() <= max {
            assert_eq!(out, text);
        } else {
            assert!(out.ends_with('…'));
        }
    }

    // Truncating already-truncated text changes nothing.
    #[hegel::test]
    fn truncate_chars_is_idempotent(tc: hegel::TestCase) {
        let text = tc.draw(generators::text());
        let max = tc.draw(generators::integers::<usize>().min_value(1).max_value(1000));
        let once = truncate_chars(text, max);
        let twice = truncate_chars(once.clone(), max);
        assert_eq!(once, twice);
    }

    // Collapsing whitespace is stable: normalized text normalizes to itself.
    #[hegel::test]
    fn normalized_is_idempotent(tc: hegel::TestCase) {
        let text = tc.draw(generators::text());
        let once = normalized([text.as_str()]);
        let twice = normalized([once.as_str()]);
        assert_eq!(once, twice);
    }
}
