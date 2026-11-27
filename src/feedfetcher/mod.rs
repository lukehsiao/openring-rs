//! The feed fetch module: turn the Feed list into Sources. It opens the
//! private feed cache once per run, asks the cache what to do for each Feed,
//! sends the request, records the [`Outcome`], parses the body into a
//! [`Source`], reports how the fetch went, and saves the cache on exit.
//! Every rule about what the cache stores lives in the private `cache`
//! submodule.

mod cache;

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use directories::ProjectDirs;
use jiff::{SignedDuration, Timestamp};
use thiserror::Error;
use tracing::{debug, warn};

use self::cache::{Body, Cache, Outcome, Plan, Validators};
use crate::{
    fan_out::{self, Words, fan_out},
    feed_list::Feed,
    fs::Fs,
    http::{Http, ReadError, Response, StatusCode, TransportError},
    source::{self, ParseError, Source},
    terminal::Terminal,
};

/// Where the feed cache lives on disk and how long entries remain valid.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CacheConfig<'a> {
    pub(crate) path: &'a Path,
    pub(crate) max_age: Duration,
}

/// Where the feed cache lives by default, if the platform has a cache
/// directory.
pub(crate) fn default_cache_path() -> Option<PathBuf> {
    ProjectDirs::from("dev", "hsiao", "openring").map(|dirs| dirs.cache_dir().join("cache.json"))
}

const FETCH_WORDS: Words = Words {
    progress: "Fetching",
    done: "Fetched",
    stale_log: "serving cached feed: fetch failed",
    failed_log: "skipping feed: fetch failed",
    panic_log: "feed fetch task failed",
};

/// Fetch every Feed through the shared `http` client as of `now`, and
/// parse each fetched body into its Source, with progress on `terminal`.
/// When `feeds` is empty, this returns immediately without reading or
/// writing the cache. Otherwise, when `cache` is `Some`, this opens the
/// cache file through `fs` before fetching and saves it afterward. A Feed
/// served from the cache because the server failed shows as stale. A Feed
/// that is gone (404 or 410), or that fails with nothing cached to fall
/// back on, is reported and skipped.
pub(crate) async fn fetch_sources(
    http: &Http,
    fs: &Fs,
    terminal: &Terminal,
    feeds: Vec<Feed>,
    cache: Option<CacheConfig<'_>>,
    now: Timestamp,
) -> Vec<Source> {
    if feeds.is_empty() {
        return Vec::new();
    }
    let cache = Cache::open(fs, cache, now);
    let items = feeds.into_iter().map(|f| (f.url.clone(), f)).collect();
    let sources = fan_out(terminal, &FETCH_WORDS, items, |_, feed| {
        // Both are cheap handles to shared state.
        let (http, cache) = (http.clone(), cache.clone());
        async move { fetch_feed(&feed, &http, &cache, now).await }
    })
    .await;
    cache.save(fs);
    sources
}

/// The largest response body accepted as a feed. Even full-content,
/// full-history feeds run single-digit MiB; anything bigger is almost
/// certainly a urls-file mistake pointing at media, and buffering it would
/// balloon memory and the on-disk cache.
const MAX_FEED_BYTES: usize = 64 * 1024 * 1024;

/// How long to wait after a 429 that names no time, or names one in neither
/// form RFC 9110 allows. The README and `--help` promise four hours.
const DEFAULT_RETRY_WINDOW: SignedDuration = SignedDuration::from_hours(4);

/// Normalize an etag so it carries the literal double quotes HTTP requires.
fn normalize_etag(s: &str) -> String {
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with("W/\"") && s.ends_with('"')) {
        s.to_string()
    } else {
        // ETag values must have the actual quotes
        format!("\"{s}\"")
    }
}

/// The instant a `Retry-After` header asks the client to wait until.
///
/// An integer that fits in an `i64`, with an optional sign, counts that
/// many seconds from `now`, stopping at the nearest instant jiff can
/// represent. An RFC 2822 date, which covers the IMF-fixdate servers send,
/// is the deadline itself. Anything else waits [`DEFAULT_RETRY_WINDOW`]:
/// no header, text, a number too long for an `i64`, and the obsolete RFC
/// 850 and asctime date forms. No value can panic a fetch.
fn retry_deadline(header: Option<&str>, now: Timestamp) -> Timestamp {
    let wait = match header.map(|h| (h, h.parse::<i64>())) {
        Some((_, Ok(secs))) => SignedDuration::from_secs(secs),
        Some((h, Err(_))) => match jiff::fmt::rfc2822::parse(h) {
            Ok(date) => return date.timestamp(),
            Err(_) => DEFAULT_RETRY_WINDOW,
        },
        None => DEFAULT_RETRY_WINDOW,
    };
    now.saturating_add(wait)
        .expect("adding a SignedDuration saturates instead of failing")
}

/// The Validators a response carries. The etag is normalized here.
fn response_validators(resp: &Response) -> Validators {
    Validators {
        etag: resp.header("etag").map(normalize_etag),
        last_modified: resp.header("last-modified").map(str::to_string),
    }
}

/// Why a fetch gave no usable feed from the server. The text goes on a
/// status line or a log line that already names the Feed, so it never
/// names the URL itself.
#[derive(Debug, Error)]
enum FetchFailure {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("server answered HTTP {}", .0.as_u16())]
    Status(StatusCode),
    #[error("rate limited (HTTP 429) with nothing cached")]
    RateLimitedUncached,
    #[error("server answered HTTP 304, but nothing is cached")]
    NotModifiedUncached,
    #[error("the body is empty")]
    Empty,
    #[error("the body is too large ({bytes} bytes)")]
    TooLarge { bytes: u64 },
    #[error(transparent)]
    NotAFeed(#[from] ParseError),
}

impl FetchFailure {
    /// Whether the server says the Feed is Gone (404 or 410). Only the user
    /// can fix that, in the urls file, and a cached copy would only hide
    /// it. Every other failure may pass by a later run.
    fn is_gone(&self) -> bool {
        matches!(self, Self::Status(StatusCode::NOT_FOUND | StatusCode::GONE))
    }
}

/// The feed to serve when the server answered, or was not asked.
enum Served {
    /// A new body, parsed before the cache stored it.
    Fresh(Source),
    /// The cached body, still current: the server confirmed it, or asked
    /// us to keep using it for a while.
    Cached(Body),
}

/// Fetch `feed` through `http`, using and updating `cache`.
/// Every cache decision and every timestamp written uses `now`.
///
/// When the server fails for a reason that may pass, this returns the
/// cached Feed as [`fan_out::Outcome::Stale`]. A Feed that is Gone, that
/// fails with nothing cached, or whose still-current cached body no longer
/// parses returns [`fan_out::Outcome::Failed`].
async fn fetch_feed(
    feed: &Feed,
    http: &Http,
    cache: &Cache,
    now: Timestamp,
) -> fan_out::Outcome<Source> {
    let served = match cache.plan(&feed.url, now) {
        Plan::Serve(body) => Ok(Served::Cached(body)),
        Plan::Request(validators) => fetch_from_server(feed, http, cache, &validators, now).await,
    };
    match served {
        Ok(Served::Fresh(source)) => fan_out::Outcome::Done(source),
        // The server sent no new body, so a cached body that no longer
        // parses leaves nothing to serve. During a Retry window, asking the
        // server anyway would ignore what it asked for.
        Ok(Served::Cached(body)) => match source::parse(&body, feed) {
            Ok(source) => fan_out::Outcome::Done(source),
            Err(e) => fan_out::Outcome::Failed(e.to_string()),
        },
        Err(failure) if failure.is_gone() => fan_out::Outcome::Failed(failure.to_string()),
        // A cached copy that does not parse cannot fill in, and the failure
        // that sent us to the cache is the one worth reporting.
        Err(failure) => match cache
            .fallback(&feed.url)
            .and_then(|body| source::parse(&body, feed).ok())
        {
            Some(source) => fan_out::Outcome::Stale(source, failure.to_string()),
            None => fan_out::Outcome::Failed(failure.to_string()),
        },
    }
}

/// Ask the server for the feed, record its answer in the cache, and say
/// which feed the answer leaves to serve.
async fn fetch_from_server(
    feed: &Feed,
    http: &Http,
    cache: &Cache,
    validators: &Validators,
    now: Timestamp,
) -> Result<Served, FetchFailure> {
    let url = &feed.url;
    let mut headers = Vec::with_capacity(2);
    if let Some(last_modified) = &validators.last_modified {
        headers.push(("if-modified-since", last_modified.as_str()));
    }
    if let Some(etag) = &validators.etag {
        headers.push(("if-none-match", etag.as_str()));
    }
    debug!(url=%url, ?headers, "sending request");

    let resp = http.get(url, &headers).await?;
    debug!(url=%url, status=%resp.status(), "received response");

    // reqwest follows redirects silently, so a moved feed keeps working
    // and the user never learns they should update their urls file.
    if resp.url() != url {
        warn!(
            from=%url.as_str(),
            to=%resp.url().as_str(),
            "feed URL redirects; consider updating it"
        );
    }

    let status = resp.status();
    let (outcome, uncached) = if status == StatusCode::NOT_MODIFIED {
        (
            Outcome::NotModified(response_validators(&resp)),
            FetchFailure::NotModifiedUncached,
        )
    } else if status.is_success() {
        let validators = response_validators(&resp);
        // Keep the raw bytes: pre-decoding to text re-encodes the transfer
        // as UTF-8 while the XML prolog still declares the original charset,
        // so the parser would decode non-UTF-8 feeds twice into mojibake.
        let body = resp
            .read_capped(MAX_FEED_BYTES)
            .await
            .map_err(|e| match e {
                ReadError::TooLarge { bytes } => FetchFailure::TooLarge { bytes },
                ReadError::Transfer(e) => e.into(),
            })?;
        // Parse before storing: a body that is empty or not a feed, such as
        // an HTML error page sent with a 200, must not replace a good cached
        // body and its Validators.
        if body.is_empty() {
            return Err(FetchFailure::Empty);
        }
        let source = source::parse(&body, feed)?;
        cache.record(url, Outcome::Fresh { body, validators }, now);
        return Ok(Served::Fresh(source));
    } else if status == StatusCode::TOO_MANY_REQUESTS {
        (
            Outcome::RateLimited {
                until: retry_deadline(resp.header("retry-after"), now),
            },
            FetchFailure::RateLimitedUncached,
        )
    } else {
        return Err(FetchFailure::Status(status));
    };
    cache
        .record(url, outcome, now)
        .map(Served::Cached)
        .ok_or(uncached)
}

#[cfg(test)]
mod tests {
    use std::{io, path::Path, time::Duration};

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use hegel::generators;
    use jiff::{SignedDuration, Timestamp, ToSpan};
    use url::Url;

    use super::{
        CacheConfig, DEFAULT_RETRY_WINDOW, MAX_FEED_BYTES, fetch_sources, normalize_etag,
        retry_deadline,
    };
    use crate::{
        feed_list::Feed,
        fs::Fs,
        http::{Answer, Http, Request},
        source::Source,
        terminal::{Line, Terminal, Tone},
        test_support::{block_on, warnings_logged},
    };

    const CACHE_PATH: &str = "/cache/cache.json";

    // A valid RSS 2.0 feed with a parameterized title, for tests that need a
    // body the parser accepts.
    fn get_valid_rss_feed(title: &str) -> String {
        format!(
            r#"
            <?xml version="1.0"?>
            <rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom">
               <channel>
                  <title>{title}</title>
                  <link>http://www.nasa.gov/</link>
                  <description>A RSS news feed containing the latest NASA press releases on the International Space Station.</description>
                  <language>en-us</language>
                  <pubDate>Tue, 10 Jun 2003 04:00:00 GMT</pubDate>
                  <item>
                     <title>Louisiana Students to Hear from NASA Astronauts Aboard Space Station</title>
                     <link>http://www.nasa.gov/press-release/louisiana-students-to-hear-from-nasa-astronauts-aboard-space-station</link>
                     <description>As part of the state's first Earth-to-space call, students from Louisiana will have an opportunity soon to hear from NASA astronauts aboard the International Space Station.</description>
                     <pubDate>Fri, 21 Jul 2023 09:04 EDT</pubDate>
                     <guid>http://www.nasa.gov/press-release/louisiana-students-to-hear-from-nasa-astronauts-aboard-space-station</guid>
                  </item>
               </channel>
            </rss>
        "#
        )
    }

    // Whatever the header holds and whenever it arrives, the deadline is an
    // instant, never a panic.
    #[hegel::test]
    fn retry_deadline_never_panics(tc: hegel::TestCase) {
        let header = tc.draw(generators::optional(generators::text()));
        let now = tc.draw(hegel::extras::jiff::timestamps());
        let _ = retry_deadline(header.as_deref(), now);
    }

    // An integer header waits that many seconds after now, stopping at the
    // nearest instant jiff can represent.
    #[hegel::test]
    fn retry_deadline_counts_integer_seconds_from_now(tc: hegel::TestCase) {
        let secs = tc.draw(generators::integers::<i64>());
        let now = tc.draw(hegel::extras::jiff::timestamps());
        let deadline = retry_deadline(Some(&secs.to_string()), now);
        // Nanoseconds, since `now` may fall between whole seconds.
        let expected = now.as_nanosecond() + i128::from(secs) * 1_000_000_000;
        let clamped = expected.clamp(
            Timestamp::MIN.as_nanosecond(),
            Timestamp::MAX.as_nanosecond(),
        );
        assert_eq!(deadline.as_nanosecond(), clamped);
    }

    // Anything that is neither delta-seconds nor an HTTP-date waits the
    // default four hours.
    #[hegel::test]
    fn retry_deadline_defaults_when_unparseable(tc: hegel::TestCase) {
        let header = tc.draw(generators::optional(generators::text()));
        tc.assume(
            header
                .as_deref()
                .is_none_or(|s| s.parse::<i64>().is_err() && jiff::fmt::rfc2822::parse(s).is_err()),
        );
        // Far enough from both ends of jiff's range that four hours fit.
        let now = Timestamp::from_second(
            tc.draw(
                generators::integers::<i64>()
                    .min_value(0)
                    .max_value(200_000_000_000),
            ),
        )
        .unwrap();
        assert_eq!(
            retry_deadline(header.as_deref(), now),
            now + DEFAULT_RETRY_WINDOW
        );
    }

    // The IMF-fixdate form servers actually send, GMT zone and all.
    #[test]
    fn retry_deadline_accepts_imf_fixdate() {
        let now: Timestamp = "2003-06-10T04:00:00Z".parse().unwrap();
        assert_eq!(
            retry_deadline(Some("Tue, 10 Jun 2003 05:00:00 GMT"), now),
            "2003-06-10T05:00:00Z".parse::<Timestamp>().unwrap()
        );
    }

    // A date-form header names its own deadline, whenever now is. A date in
    // the past closes the window at once.
    #[hegel::test]
    fn retry_deadline_is_the_http_date_itself(tc: hegel::TestCase) {
        // RFC 2822 prints only four-digit positive years, so dates run from
        // 1970 to the last whole second jiff can represent.
        let date = Timestamp::from_second(
            tc.draw(
                generators::integers::<i64>()
                    .min_value(0)
                    .max_value(Timestamp::MAX.as_second()),
            ),
        )
        .unwrap();
        let now = tc.draw(hegel::extras::jiff::timestamps());
        let header =
            jiff::fmt::rfc2822::to_string(&date.to_zoned(jiff::tz::TimeZone::UTC)).unwrap();
        assert_eq!(retry_deadline(Some(&header), now), date);
    }

    // A server's etag compares equal however many times it passes through,
    // so the If-None-Match sent back is exactly what the server issued.
    #[hegel::test]
    fn normalize_etag_quotes_once(tc: hegel::TestCase) {
        let raw = tc.draw(generators::text());
        let once = normalize_etag(&raw);
        assert_eq!(normalize_etag(&once), once);
    }

    fn now() -> Timestamp {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }

    fn cache_config() -> CacheConfig<'static> {
        CacheConfig {
            path: Path::new(CACHE_PATH),
            max_age: Duration::MAX,
        }
    }

    fn feed_url() -> Url {
        Url::parse("https://feed.example/feed.xml").unwrap()
    }

    fn feed_at(url: Url) -> Feed {
        Feed { url, weight: None }
    }

    fn feed() -> Feed {
        feed_at(feed_url())
    }

    fn title(source: &Source) -> &str {
        source
            .articles()
            .first()
            .expect("feed has an article")
            .source_title()
    }

    fn request_at(url: Url, headers: &[(&'static str, &str)]) -> Request {
        Request {
            url,
            headers: headers
                .iter()
                .map(|&(name, value)| (name, value.to_string()))
                .collect(),
        }
    }

    fn request(headers: &[(&'static str, &str)]) -> Request {
        request_at(feed_url(), headers)
    }

    fn fetched_line(url: &Url) -> Line {
        Line {
            word: "Fetched",
            tone: Tone::Good,
            detail: url.to_string(),
        }
    }

    fn error_line(url: &Url, cause: &str) -> Line {
        Line {
            word: "Error",
            tone: Tone::Bad,
            detail: format!("{url} ({cause})"),
        }
    }

    // Build an on-disk cache JSON entry so tests can seed invalid cached bytes
    // through `Fs`.
    fn cache_json(
        url: &Url,
        body: &[u8],
        etag: Option<&str>,
        fetched_at: Timestamp,
        retry_until: Option<Timestamp>,
    ) -> String {
        serde_json::json!({
            url.as_str(): {
                "fetched_at": fetched_at,
                "retry_until": retry_until,
                "etag": etag,
                "last_modified": serde_json::Value::Null,
                "body": STANDARD.encode(body),
            }
        })
        .to_string()
    }

    struct FetchRun {
        sources: Vec<Source>,
        lines: Vec<Line>,
    }

    async fn run_fetch(
        http: &Http,
        fs: &Fs,
        feeds: Vec<Feed>,
        cache: Option<CacheConfig<'_>>,
        at: Timestamp,
    ) -> FetchRun {
        let terminal = Terminal::create_null(true);
        let lines = terminal.track_lines();
        let sources = fetch_sources(http, fs, &terminal, feeds, cache, at).await;
        FetchRun {
            sources,
            lines: lines.data(),
        }
    }

    fn run_fetch_logged(
        http: &Http,
        fs: &Fs,
        feeds: Vec<Feed>,
        cache: Option<CacheConfig<'_>>,
        at: Timestamp,
    ) -> (FetchRun, String) {
        let mut run = None;
        let logs = warnings_logged(|| {
            run = Some(block_on(run_fetch(http, fs, feeds, cache, at)));
        });
        (run.expect("warnings_logged runs its closure"), logs)
    }

    // Seed the cache with a Feed titled "cached" and ETag "v1", then run
    // `answer` against it. Verify the run serves the cached copy as Stale
    // and keeps its Validators for the next run.
    async fn assert_served_stale(answer: Answer, is_cause: impl Fn(&str) -> bool) {
        let fs = Fs::create_null([]);
        let http = Http::create_null([(
            feed_url(),
            vec![
                Answer::ok(get_valid_rss_feed("cached")).header("etag", "\"v1\""),
                answer,
                Answer::status(304),
            ],
        )]);
        let requests = http.track_requests();

        let first = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert_eq!(first.sources.len(), 1);
        assert_eq!(title(&first.sources[0]), "cached");
        assert_eq!(first.lines, [fetched_line(&feed_url())]);

        let second = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert_eq!(second.sources.len(), 1);
        assert_eq!(title(&second.sources[0]), "cached");
        let [line] = second.lines.as_slice() else {
            panic!("expected one status line, got {:?}", second.lines);
        };
        assert_eq!((line.word, line.tone), ("Stale", Tone::Warn));
        let prefix = format!("{} (", feed_url());
        let cause = line
            .detail
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(')'))
            .unwrap_or_else(|| panic!("unexpected stale detail: {:?}", line.detail));
        assert!(is_cause(cause), "{cause:?}");

        let third = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert_eq!(third.sources.len(), 1);
        assert_eq!(title(&third.sources[0]), "cached");
        assert_eq!(third.lines, [fetched_line(&feed_url())]);

        assert_eq!(
            requests.data(),
            [
                request(&[]),
                request(&[("if-none-match", "\"v1\"")]),
                request(&[("if-none-match", "\"v1\"")]),
            ]
        );
    }

    #[tokio::test]
    async fn stores_normalized_validators_and_serves_the_cache_on_304() {
        let fs = Fs::create_null([]);
        let http = Http::create_null([(
            feed_url(),
            vec![
                Answer::ok(get_valid_rss_feed("fresh"))
                    .header("etag", "feed-etag")
                    .header("last-modified", "Mon, 01 Jan 2024 00:00:00 GMT"),
                Answer::status(304),
            ],
        )]);
        let requests = http.track_requests();

        let first = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert_eq!(first.sources.len(), 1);
        assert_eq!(title(&first.sources[0]), "fresh");
        assert_eq!(first.lines, [fetched_line(&feed_url())]);

        let second = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert_eq!(second.sources.len(), 1);
        assert_eq!(title(&second.sources[0]), "fresh");
        assert_eq!(second.lines, [fetched_line(&feed_url())]);

        assert_eq!(
            requests.data(),
            [
                request(&[]),
                request(&[
                    ("if-modified-since", "Mon, 01 Jan 2024 00:00:00 GMT"),
                    ("if-none-match", "\"feed-etag\""),
                ]),
            ]
        );
    }

    #[tokio::test]
    async fn a_304_rotates_the_validators_for_the_next_run() {
        let fs = Fs::create_null([]);
        // RFC 7232 allows a 304 to rotate the Validators it confirms.
        let http = Http::create_null([(
            feed_url(),
            vec![
                Answer::ok(get_valid_rss_feed("cached")).header("etag", "\"old\""),
                Answer::status(304).header("etag", "\"new\""),
                Answer::status(304),
            ],
        )]);
        let requests = http.track_requests();

        for _ in 0..3 {
            let run = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
            assert_eq!(run.sources.len(), 1);
            assert_eq!(title(&run.sources[0]), "cached");
            assert_eq!(run.lines, [fetched_line(&feed_url())]);
        }

        assert_eq!(
            requests.data(),
            [
                request(&[]),
                request(&[("if-none-match", "\"old\"")]),
                request(&[("if-none-match", "\"new\"")]),
            ]
        );
    }

    #[tokio::test]
    async fn a_304_after_an_expired_retry_window_does_not_reopen_it() {
        let t0 = now();
        let fs = Fs::create_null([]);
        let http = Http::create_null([(
            feed_url(),
            vec![
                Answer::ok(get_valid_rss_feed("cached")).header("etag", "\"v1\""),
                Answer::status(429).header("retry-after", "3600"),
                Answer::status(304),
                Answer::status(304),
            ],
        )]);
        let requests = http.track_requests();

        for at in [t0, t0, t0 + 2.hours(), t0 + 2.hours() + 1.second()] {
            let run = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), at).await;
            assert_eq!(run.sources.len(), 1);
            assert_eq!(title(&run.sources[0]), "cached");
            assert_eq!(run.lines, [fetched_line(&feed_url())]);
        }

        // Four runs, four requests: a 304 ends the Retry window just as a
        // 200 does, so the fourth run asks the server again.
        assert_eq!(requests.data().len(), 4);
    }

    #[tokio::test]
    async fn decodes_non_utf8_feeds_per_their_xml_prolog() {
        // A real ISO-8859-1 feed: 0xE9 is é, and the prolog declares the
        // encoding. Decoding the transfer as UTF-8 first would corrupt the
        // byte before the XML parser ever saw the prolog.
        let mut body = br#"<?xml version="1.0" encoding="ISO-8859-1"?>
<rss version="2.0"><channel><title>caf"#
            .to_vec();
        body.push(0xE9);
        body.extend_from_slice(
            br"</title><link>https://x.example/</link><description>d</description><item><title>post</title><link>https://x.example/post</link><pubDate>Tue, 10 Jun 2003 04:00:00 GMT</pubDate></item></channel></rss>",
        );
        let fs = Fs::create_null([]);
        let http = Http::create_null([(feed_url(), vec![Answer::ok(body), Answer::status(304)])]);

        let first = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert_eq!(first.sources.len(), 1);
        assert_eq!(title(&first.sources[0]), "caf\u{e9}");

        // The cached bytes must survive disk serialization and reparse
        // identically on a later 304.
        let second = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert_eq!(second.sources.len(), 1);
        assert_eq!(title(&second.sources[0]), "caf\u{e9}");
    }

    #[tokio::test]
    async fn streamed_bodies_past_the_cap_are_rejected_and_not_cached() {
        // No Content-Length, the shape a chunked or compressed transfer
        // takes. Only the streamed read cap catches this.
        let fs = Fs::create_null([]);
        let http = Http::create_null([(
            feed_url(),
            vec![
                Answer::status(200)
                    .header("etag", "\"huge\"")
                    .chunks(vec![vec![b'x'; MAX_FEED_BYTES], vec![b'x']])
                    .chunked(),
                Answer::status(503),
            ],
        )]);
        let requests = http.track_requests();

        let first = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        let expected_bytes = u64::try_from(MAX_FEED_BYTES).unwrap() + 1;
        assert!(first.sources.is_empty(), "{:?}", first.sources);
        assert_eq!(
            first.lines,
            [error_line(
                &feed_url(),
                &format!("the body is too large ({expected_bytes} bytes)")
            )]
        );

        // Nothing was cached, so the next run sends no Validators and has no
        // Stale copy to serve on 503.
        let second = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert!(second.sources.is_empty(), "{:?}", second.sources);
        assert_eq!(requests.data(), [request(&[]), request(&[])]);
    }

    #[tokio::test]
    async fn oversized_feeds_are_rejected_by_their_announced_length() {
        // A urls-file typo that points at a video or other huge file.
        let announced = u64::try_from(MAX_FEED_BYTES).unwrap() + 1;
        let fs = Fs::create_null([]);
        let http = Http::create_null([(
            feed_url(),
            vec![
                Answer::ok(get_valid_rss_feed("huge"))
                    .header("etag", "\"huge\"")
                    .content_length(announced),
                Answer::status(503),
            ],
        )]);
        let requests = http.track_requests();

        let first = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert!(first.sources.is_empty(), "{:?}", first.sources);
        assert_eq!(
            first.lines,
            [error_line(
                &feed_url(),
                &format!("the body is too large ({announced} bytes)")
            )]
        );

        // Nothing that big belongs in the cache either.
        let second = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert!(second.sources.is_empty(), "{:?}", second.sources);
        assert_eq!(requests.data(), [request(&[]), request(&[])]);
    }

    #[tokio::test]
    async fn a_failed_request_serves_the_cache() {
        assert_served_stale(Answer::send_error("connection refused"), |cause| {
            cause == "connection refused"
        })
        .await;
    }

    #[tokio::test]
    async fn a_failed_request_with_nothing_cached_is_an_error() {
        let fs = Fs::create_null([]);
        let http = Http::create_null([(feed_url(), Answer::send_error("connection refused"))]);

        let run = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert!(run.sources.is_empty(), "{:?}", run.sources);
        assert_eq!(run.lines, [error_line(&feed_url(), "connection refused")]);
    }

    #[tokio::test]
    async fn a_body_that_stops_arriving_serves_the_cache() {
        assert_served_stale(
            Answer::ok(get_valid_rss_feed("partial"))
                .header("etag", "\"v2\"")
                .transfer_error("connection reset"),
            |cause| cause == "connection reset",
        )
        .await;
    }

    // A 200 that carries an HTML error page instead of a feed must not
    // replace the good copy. If it did, its Validators would go out next
    // run, the server would answer 304, and the error page would be served
    // until it changed.
    #[tokio::test]
    async fn a_200_that_is_not_a_feed_serves_the_cache_and_keeps_it() {
        assert_served_stale(
            Answer::ok("<html><body>Service Unavailable</body></html>").header("etag", "\"v2\""),
            |cause| cause.contains("unable to parse"),
        )
        .await;
    }

    #[tokio::test]
    async fn an_empty_200_serves_the_cache_and_keeps_it() {
        assert_served_stale(Answer::ok("").header("etag", "\"v2\""), |cause| {
            cause == "the body is empty"
        })
        .await;
    }

    #[tokio::test]
    async fn an_oversized_answer_serves_the_cache() {
        let announced = u64::try_from(MAX_FEED_BYTES).unwrap() + 1;
        assert_served_stale(
            Answer::ok("big")
                .header("etag", "\"v2\"")
                .content_length(announced),
            |cause| cause == format!("the body is too large ({announced} bytes)"),
        )
        .await;
    }

    // Any status that is not a feed, a 304, a 429, or a sign the feed is
    // Gone may pass by a later run, so the cached copy fills in.
    #[hegel::test]
    fn a_failing_status_serves_the_cache(tc: hegel::TestCase) {
        let code = tc.draw(generators::integers::<u16>().min_value(300).max_value(599));
        tc.assume(![304, 404, 410, 429].contains(&code));
        block_on(assert_served_stale(Answer::status(code), |cause| {
            cause == format!("server answered HTTP {code}")
        }));
    }

    // A 404 or 410 says the Feed is Gone. Only the user can fix that, so
    // the cached copy must not hide it.
    #[tokio::test]
    async fn a_gone_feed_shows_an_error_and_yields_no_source() {
        let huge = u64::try_from(MAX_FEED_BYTES).unwrap() + 1;
        for code in [404, 410] {
            // An announced body too large to read changes nothing: the
            // status alone says the Feed is Gone.
            for gone_answer in [
                Answer::status(code),
                Answer::status(code).content_length(huge),
            ] {
                let fs = Fs::create_null([]);
                let http = Http::create_null([(
                    feed_url(),
                    vec![Answer::ok(get_valid_rss_feed("cached")), gone_answer],
                )]);

                let first = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
                assert_eq!(first.sources.len(), 1);

                let second = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
                assert!(second.sources.is_empty(), "{code}: {:?}", second.sources);
                assert_eq!(
                    second.lines,
                    [error_line(
                        &feed_url(),
                        &format!("server answered HTTP {code}")
                    )]
                );
            }
        }
    }

    // A 429 that announces a huge body is still a rate limit: the cache
    // serves its copy as current and remembers the Retry window.
    #[tokio::test]
    async fn a_429_announcing_a_huge_body_still_opens_a_retry_window() {
        let huge = u64::try_from(MAX_FEED_BYTES).unwrap() + 1;
        let fs = Fs::create_null([]);
        let http = Http::create_null([(
            feed_url(),
            vec![
                Answer::ok(get_valid_rss_feed("cached")),
                Answer::status(429)
                    .header("retry-after", "120")
                    .content_length(huge),
            ],
        )]);
        let requests = http.track_requests();

        let first = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert_eq!(title(&first.sources[0]), "cached");

        let second = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert_eq!(title(&second.sources[0]), "cached");
        assert_eq!(second.lines, [fetched_line(&feed_url())]);

        let third = run_fetch(
            &http,
            &fs,
            vec![feed()],
            Some(cache_config()),
            now() + 119.seconds(),
        )
        .await;
        assert_eq!(title(&third.sources[0]), "cached");
        assert_eq!(third.lines, [fetched_line(&feed_url())]);
        assert_eq!(requests.data().len(), 2);
    }

    // When the cached copy itself does not parse, it cannot fill in, and
    // the error reported is the server's, not the parser's.
    #[test]
    fn an_unparseable_cached_copy_reports_the_original_failure() {
        let seeded = cache_json(
            &feed_url(),
            b"<html>not a feed</html>",
            Some("\"v1\""),
            now(),
            None,
        );
        let fs = Fs::create_null([(CACHE_PATH, seeded.as_str())]);
        let http = Http::create_null([(feed_url(), Answer::status(503))]);
        let requests = http.track_requests();

        let (run, logs) = run_fetch_logged(&http, &fs, vec![feed()], Some(cache_config()), now());

        // Confirm the seeded entry loaded cleanly by checking for zero cache
        // warnings and the stored ETag on the outbound request.
        assert_eq!(logs, "");
        assert_eq!(requests.data(), [request(&[("if-none-match", "\"v1\"")])]);
        assert!(run.sources.is_empty(), "{:?}", run.sources);
        assert_eq!(
            run.lines,
            [error_line(&feed_url(), "server answered HTTP 503")]
        );
    }

    #[tokio::test]
    async fn an_empty_body_is_not_cached_as_servable() {
        // A misbehaving feed: 200 with an etag but a completely empty body.
        let fs = Fs::create_null([]);
        let http = Http::create_null([(feed_url(), Answer::ok("").header("etag", "\"empty\""))]);
        let requests = http.track_requests();

        let first = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert!(first.sources.is_empty(), "{:?}", first.sources);
        assert_eq!(first.lines, [error_line(&feed_url(), "the body is empty")]);

        // The empty body must not be cached. If it were, the next fetch
        // would send If-None-Match, the server would say 304, and the run
        // would serve the empty body until the entry aged out of the cache.
        let second = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert!(second.sources.is_empty(), "{:?}", second.sources);
        assert_eq!(second.lines, [error_line(&feed_url(), "the body is empty")]);
        assert_eq!(
            requests.data(),
            [request(&[]), request(&[])],
            "second fetch sent a conditional request with nothing to serve"
        );
    }

    #[tokio::test]
    async fn a_429_serves_the_cache_until_its_retry_after_passes() {
        let fs = Fs::create_null([]);
        let http = Http::create_null([(
            feed_url(),
            vec![
                Answer::ok(get_valid_rss_feed("rate-limited")).header("etag", "\"v1\""),
                Answer::status(429).header("retry-after", "120"),
                Answer::ok(get_valid_rss_feed("recovered")),
            ],
        )]);
        let requests = http.track_requests();

        let first = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert_eq!(title(&first.sources[0]), "rate-limited");

        let second = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert_eq!(title(&second.sources[0]), "rate-limited");
        assert_eq!(second.lines, [fetched_line(&feed_url())]);

        // Inside the Retry window, fetch_sources serves the cached Feed
        // without sending a request.
        let during = run_fetch(
            &http,
            &fs,
            vec![feed()],
            Some(cache_config()),
            now() + 119.seconds(),
        )
        .await;
        assert_eq!(title(&during.sources[0]), "rate-limited");
        assert_eq!(during.lines, [fetched_line(&feed_url())]);
        assert_eq!(requests.data().len(), 2);

        // After the Retry window expires, fetch_sources asks the server again
        // with the stored Validators.
        let after = run_fetch(
            &http,
            &fs,
            vec![feed()],
            Some(cache_config()),
            now() + 120.seconds(),
        )
        .await;
        assert_eq!(title(&after.sources[0]), "recovered");
        assert_eq!(after.lines, [fetched_line(&feed_url())]);
        assert_eq!(
            requests.data(),
            [
                request(&[]),
                request(&[("if-none-match", "\"v1\"")]),
                request(&[("if-none-match", "\"v1\"")]),
            ]
        );
    }

    #[tokio::test]
    async fn a_429_with_an_http_date_waits_until_that_date() {
        // RFC 9110 allows Retry-After to be an HTTP-date as well as seconds.
        let until = now() + 600.seconds();
        let header =
            jiff::fmt::rfc2822::to_string(&until.to_zoned(jiff::tz::TimeZone::UTC)).unwrap();
        let fs = Fs::create_null([]);
        let http = Http::create_null([(
            feed_url(),
            vec![
                Answer::ok(get_valid_rss_feed("rate-limited")),
                Answer::status(429).header("retry-after", &header),
                Answer::ok(get_valid_rss_feed("recovered")),
            ],
        )]);
        let requests = http.track_requests();

        let first = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert_eq!(title(&first.sources[0]), "rate-limited");

        let second = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert_eq!(title(&second.sources[0]), "rate-limited");

        let during = run_fetch(
            &http,
            &fs,
            vec![feed()],
            Some(cache_config()),
            until - SignedDuration::from_secs(1),
        )
        .await;
        assert_eq!(title(&during.sources[0]), "rate-limited");
        assert_eq!(requests.data().len(), 2);

        let after = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), until).await;
        assert_eq!(title(&after.sources[0]), "recovered");
        assert_eq!(requests.data().len(), 3);
    }

    // A cached body that no longer parses, as after a parser upgrade, fails
    // the fetch during a Retry window. The server asked not to be asked, so
    // no request goes out.
    #[test]
    fn an_open_retry_window_over_an_unparseable_body_fails_without_a_request() {
        let seeded = cache_json(
            &feed_url(),
            b"not a feed",
            None,
            now(),
            Some(now() + 1.hour()),
        );
        let fs = Fs::create_null([(CACHE_PATH, seeded.as_str())]);
        let http = Http::create_null::<Answer>([]);
        let requests = http.track_requests();

        let (run, logs) = run_fetch_logged(
            &http,
            &fs,
            vec![feed()],
            Some(cache_config()),
            now() + 30.minutes(),
        );

        assert_eq!(logs, "");
        assert_eq!(requests.data(), []);
        assert!(run.sources.is_empty(), "{:?}", run.sources);
        let [line] = run.lines.as_slice() else {
            panic!("expected one status line, got {:?}", run.lines);
        };
        assert_eq!((line.word, line.tone), ("Error", Tone::Bad));
        assert!(line.detail.starts_with(feed_url().as_str()), "{line:?}");
        assert!(line.detail.contains("unable to parse"), "{line:?}");
    }

    #[tokio::test]
    async fn a_429_without_a_cached_copy_is_a_failure() {
        let expected = error_line(&feed_url(), "rate limited (HTTP 429) with nothing cached");

        // With caching enabled but nothing stored yet.
        let fs = Fs::create_null([]);
        let http = Http::create_null([(feed_url(), Answer::status(429))]);
        let uncached = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert!(uncached.sources.is_empty(), "{:?}", uncached.sources);
        assert_eq!(uncached.lines, std::slice::from_ref(&expected));

        // With caching disabled, even a prior 200 leaves nothing cached.
        let http = Http::create_null([(
            feed_url(),
            vec![Answer::ok(get_valid_rss_feed("fresh")), Answer::status(429)],
        )]);
        let first = run_fetch(&http, &fs, vec![feed()], None, now()).await;
        assert_eq!(title(&first.sources[0]), "fresh");

        let no_cache = run_fetch(&http, &fs, vec![feed()], None, now()).await;
        assert!(no_cache.sources.is_empty(), "{:?}", no_cache.sources);
        assert_eq!(no_cache.lines, std::slice::from_ref(&expected));
    }

    // A 304 to a request that sent no Validators confirms nothing the cache
    // holds. It is not an empty feed.
    #[tokio::test]
    async fn a_304_for_an_unknown_feed_is_a_failure() {
        let fs = Fs::create_null([]);
        let http = Http::create_null([(feed_url(), Answer::status(304))]);

        let run = run_fetch(&http, &fs, vec![feed()], Some(cache_config()), now()).await;
        assert!(run.sources.is_empty(), "{:?}", run.sources);
        assert_eq!(
            run.lines,
            [error_line(
                &feed_url(),
                "server answered HTTP 304, but nothing is cached"
            )]
        );
    }

    #[test]
    fn a_redirected_feed_warns_and_stays_cached_under_the_requested_url() {
        let moved = Url::parse("https://feed.example/new.xml").unwrap();
        let fs = Fs::create_null([]);
        let http = Http::create_null([
            (
                feed_url(),
                vec![
                    Answer::ok(get_valid_rss_feed("moved"))
                        .header("etag", "\"v1\"")
                        .redirected_to(moved.clone()),
                    Answer::status(304),
                ],
            ),
            (moved.clone(), vec![Answer::ok(get_valid_rss_feed("moved"))]),
        ]);
        let requests = http.track_requests();

        let (first, logs) = run_fetch_logged(&http, &fs, vec![feed()], Some(cache_config()), now());
        assert_eq!(first.sources.len(), 1);
        assert_eq!(title(&first.sources[0]), "moved");
        assert_eq!(first.lines, [fetched_line(&feed_url())]);
        assert!(
            logs.contains("feed URL redirects; consider updating it"),
            "{logs}"
        );
        assert!(logs.contains(&format!("from={}", feed_url())), "{logs}");
        assert!(logs.contains(&format!("to={moved}")), "{logs}");

        // The cache stays keyed by the URL the user configured, so later runs
        // revalidate it while a fetch of the redirect target carries no
        // Validators.
        let second = block_on(run_fetch(
            &http,
            &fs,
            vec![feed()],
            Some(cache_config()),
            now(),
        ));
        assert_eq!(second.sources.len(), 1);
        assert_eq!(title(&second.sources[0]), "moved");

        let target = block_on(run_fetch(
            &http,
            &fs,
            vec![feed_at(moved.clone())],
            Some(cache_config()),
            now(),
        ));
        assert_eq!(target.sources.len(), 1);
        assert_eq!(title(&target.sources[0]), "moved");

        assert_eq!(
            requests.data(),
            [
                request(&[]),
                request(&[("if-none-match", "\"v1\"")]),
                request_at(moved, &[]),
            ]
        );
    }

    #[test]
    fn unexpected_status_is_an_error() {
        let fs = Fs::create_null([]);
        let http = Http::create_null([(feed_url(), Answer::status(500))]);

        let visible = block_on(run_fetch(
            &http,
            &fs,
            vec![feed()],
            Some(cache_config()),
            now(),
        ));
        assert!(visible.sources.is_empty(), "{:?}", visible.sources);
        assert_eq!(
            visible.lines,
            [error_line(&feed_url(), "server answered HTTP 500")]
        );

        let logs = warnings_logged(|| {
            let sources = block_on(fetch_sources(
                &http,
                &fs,
                &Terminal::create_null(false),
                vec![feed()],
                Some(cache_config()),
                now(),
            ));
            assert!(sources.is_empty(), "{sources:?}");
        });
        assert!(logs.contains("skipping feed: fetch failed"), "{logs}");
        assert!(logs.contains("server answered HTTP 500"), "{logs}");
    }

    // An empty Feed list returns immediately without reading or writing the
    // cache file.
    #[test]
    fn fetch_sources_with_no_feeds_skips_cache_disk_io() {
        let cache_path = Path::new(CACHE_PATH);
        let cache = Some(cache_config());

        let corrupt_fs = Fs::create_null([(CACHE_PATH, "{corrupt")]);
        let logs = warnings_logged(|| {
            let sources = block_on(fetch_sources(
                &Http::create_null::<Answer>([]),
                &corrupt_fs,
                &Terminal::create_null(false),
                Vec::new(),
                cache,
                now(),
            ));
            assert!(sources.is_empty());
        });
        assert_eq!(logs, "");
        assert_eq!(corrupt_fs.read_locked(cache_path).unwrap(), "{corrupt");

        let empty_fs = Fs::create_null([]);
        let sources = block_on(fetch_sources(
            &Http::create_null::<Answer>([]),
            &empty_fs,
            &Terminal::create_null(false),
            Vec::new(),
            cache,
            now(),
        ));
        assert!(sources.is_empty());
        assert_eq!(
            empty_fs.read_locked(cache_path).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
