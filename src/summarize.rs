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

    // Content-Length cannot be trusted for chunked or compressed responses,
    // so the cap applies to the bytes as they arrive.
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

    use super::*;
    use crate::{http::Answer, test_support::block_on};

    #[test]
    fn is_html_accepts_html_content_types() {
        assert!(is_html(None), "absent header gets the benefit of the doubt");
        assert!(is_html(Some("text/html")));
        assert!(is_html(Some("text/html; charset=utf-8")));
        assert!(is_html(Some("application/xhtml+xml")));
        assert!(is_html(Some("TEXT/HTML")), "matching is case-insensitive");
    }

    #[test]
    fn is_html_rejects_non_html_content_types() {
        assert!(!is_html(Some("application/json")));
        assert!(!is_html(Some("image/png")));
        assert!(!is_html(Some("application/rss+xml")));
    }

    #[test]
    fn meta_description_is_preferred_over_body_paragraphs() {
        let html = r#"<html><head>
            <meta name="description" content="Meta wins">
            </head><body><article><p>Body paragraph</p></article></body></html>"#;
        assert_eq!(extract_summary(html).as_deref(), Some("Meta wins"));
    }

    #[test]
    fn standard_description_beats_social_variants() {
        let html = r#"<html><head>
            <meta property="og:description" content="OG desc">
            <meta name="description" content="Standard desc">
            </head><body></body></html>"#;
        assert_eq!(extract_summary(html).as_deref(), Some("Standard desc"));
    }

    #[test]
    fn og_description_used_when_no_standard_description() {
        let html = r#"<html><head>
            <meta property="og:description" content="OG desc">
            </head><body></body></html>"#;
        assert_eq!(extract_summary(html).as_deref(), Some("OG desc"));
    }

    #[test]
    fn twitter_description_used_as_last_meta_resort() {
        let html = r#"<html><head>
            <meta name="twitter:description" content="Tweet desc">
            </head><body></body></html>"#;
        assert_eq!(extract_summary(html).as_deref(), Some("Tweet desc"));
    }

    #[test]
    fn leading_paragraphs_used_when_no_meta_description() {
        let html = r"<html><body><article>
            <p>First   line.</p>
            <p>Second line.</p>
            </article></body></html>";
        assert_eq!(
            extract_summary(html).as_deref(),
            Some("First line. Second line.")
        );
    }

    #[test]
    fn article_scope_keeps_navigation_out_of_the_summary() {
        let html = r"<html><body>
            <nav><p>Home About Contact</p></nav>
            <article><p>The actual article body.</p></article>
            </body></html>";
        assert_eq!(
            extract_summary(html).as_deref(),
            Some("The actual article body.")
        );
    }

    #[test]
    fn script_bodies_do_not_leak_into_the_summary() {
        let html = r"<html><body>
            <script>var tracking = 1;</script>
            <p>Readable text.</p>
            </body></html>";
        assert_eq!(extract_summary(html).as_deref(), Some("Readable text."));
    }

    #[test]
    fn empty_page_yields_no_summary() {
        let html = "<html><head></head><body></body></html>";
        assert_eq!(extract_summary(html), None);
    }

    #[test]
    fn whitespace_only_meta_falls_back_to_paragraphs() {
        let html = r#"<html><head>
            <meta name="description" content="   ">
            </head><body><p>Fallback body.</p></body></html>"#;
        assert_eq!(extract_summary(html).as_deref(), Some("Fallback body."));
    }

    #[test]
    fn long_summaries_are_truncated_with_an_ellipsis() {
        // 1000 characters, well over the cap.
        let long = "word ".repeat(200);
        let html = format!(
            r#"<html><head><meta name="description" content="{long}"></head><body></body></html>"#
        );
        let summary = extract_summary(&html).expect("meta description present");
        assert!(summary.chars().count() <= MAX_SUMMARY_CHARS);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn empty_paragraphs_are_skipped() {
        let html = r"<html><body><article>
            <p></p>
            <p>   </p>
            <p>Real content.</p>
            </article></body></html>";
        assert_eq!(extract_summary(html).as_deref(), Some("Real content."));
    }

    #[test]
    fn paragraph_accumulation_stops_at_the_cap() {
        // Each paragraph runs 81 characters, so a handful blow past the cap
        // and accumulation stops early with a truncated result.
        let para = format!("<p>{}</p>", "lorem ipsum dolor sit amet ".repeat(3));
        let body = para.repeat(20);
        let html = format!("<html><body><article>{body}</article></body></html>");
        let summary = extract_summary(&html).expect("paragraphs present");
        assert!(summary.chars().count() <= MAX_SUMMARY_CHARS);
        assert!(summary.ends_with('…'));
    }

    fn post_url() -> Url {
        Url::parse("https://blog.example/post").unwrap()
    }

    fn summarize(answer: Answer) -> Option<String> {
        let http = Http::create_null([(post_url(), answer)]);
        block_on(fetch_summary(&http, &post_url()))
    }

    #[test]
    fn an_html_page_yields_its_summary() {
        let page =
            r#"<html><head><meta name="description" content="About this post."></head></html>"#;
        assert_eq!(
            summarize(Answer::ok(page).header("content-type", "text/html; charset=utf-8"))
                .as_deref(),
            Some("About this post.")
        );
    }

    #[test]
    fn non_html_pages_yield_no_summary() {
        let answer = Answer::ok(r#"{"summary":"nope"}"#).header("content-type", "application/json");
        assert_eq!(summarize(answer), None);
    }

    #[test]
    fn oversized_pages_yield_no_summary() {
        let page = format!(
            r#"<html><head><meta name="description" content="Huge."></head><body>{}</body></html>"#,
            "x".repeat(MAX_PAGE_BYTES)
        );
        assert_eq!(summarize(Answer::ok(page).chunked()), None);
    }

    #[test]
    fn unreachable_pages_yield_no_summary() {
        assert_eq!(summarize(Answer::send_error("connection refused")), None);
    }

    // A page that answers with an error status yields nothing, even when
    // the error page itself carries a description.
    #[hegel::test]
    fn error_statuses_yield_no_summary(tc: hegel::TestCase) {
        let status = tc.draw(generators::integers::<u16>().min_value(400).max_value(599));
        let page = r#"<html><head><meta name="description" content="Not Found"></head></html>"#;
        assert_eq!(summarize(Answer::status(status).body(page)), None);
    }

    // Whatever the page holds, deriving a summary yields text or nothing but
    // never panics.
    #[hegel::test]
    fn extract_summary_never_panics(tc: hegel::TestCase) {
        let html = tc.draw(generators::text());
        let _ = extract_summary(&html);
    }

    // A truncated summary never exceeds the cap, is left untouched when it
    // already fits, and is marked with an ellipsis when it is cut.
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
