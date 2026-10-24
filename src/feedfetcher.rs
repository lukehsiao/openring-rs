//! The feed fetch module: turn the Feed list into Sources. For each Feed it
//! asks the cache what to do, sends the request, turns the response into a
//! cache [`Outcome`], parses the body the cache hands back, and reports how
//! the fetch went. Every rule about what the cache stores lives in the
//! cache module.

use feed_rs::{model::Feed as Document, parser};
use jiff::{SignedDuration, Timestamp};
use tracing::{debug, warn};
use url::Url;

use crate::{
    cache::{Body, Cache, Outcome, Plan, Validators},
    error::OpenringError,
    fan_out::{self, Words, fan_out},
    feed_list::Feed,
    http::{Http, ReadError, Response, StatusCode},
    source::{self, Source},
    terminal::Terminal,
};

const FETCH_WORDS: Words = Words {
    progress: "Fetching",
    done: "Fetched",
    stale_log: "serving cached feed: fetch failed",
    failed_log: "skipping feed: fetch failed",
    panic_log: "feed fetch task failed",
};

/// Fetch every Feed through the shared `http` client as of `now`, and
/// project each fetched document onto its Source, with progress on
/// `terminal`. A Feed served from the cache because the server failed shows
/// as stale. A Feed that is gone (404 or 410), or that fails with nothing
/// cached to fall back on, is reported and skipped.
pub(crate) async fn fetch_sources(
    http: &Http,
    terminal: &Terminal,
    feeds: Vec<Feed>,
    cache: &Cache,
    now: Timestamp,
) -> Vec<Source> {
    let items = feeds.into_iter().map(|f| (f.url.clone(), f)).collect();
    fan_out(terminal, &FETCH_WORDS, items, |url, feed| {
        // Both are cheap handles to shared state.
        let (http, cache) = (http.clone(), cache.clone());
        async move {
            match fetch_feed(&url, &http, &cache, now).await {
                Ok(Fetched {
                    document,
                    stale_because: None,
                }) => fan_out::Outcome::Done(source::project(&document, feed)),
                Ok(Fetched {
                    document,
                    stale_because: Some(e),
                }) => fan_out::Outcome::Stale(source::project(&document, feed), e.to_string()),
                Err(e) => fan_out::Outcome::Failed(e.to_string()),
            }
        }
    })
    .await
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

/// A parsed feed document for this run.
#[derive(Debug)]
struct Fetched {
    document: Document,
    /// Why the server could not be used, when the feed came from the cache
    /// instead. `None` when the feed counts as current: the server sent or
    /// confirmed it, or a Retry window covers it.
    stale_because: Option<OpenringError>,
}

/// Why the server did not give a usable feed.
enum Failure {
    /// The server says the feed is gone (404 or 410). Only the user can fix
    /// that, in the urls file, and a cached copy would only hide it.
    Gone(OpenringError),
    /// Anything that may pass by a later run. The cached copy fills in.
    Passing(OpenringError),
}

/// The feed to serve when the server answered, or was not asked.
#[expect(
    clippy::large_enum_variant,
    reason = "one short-lived value per fetch, so boxing would only add an allocation"
)]
enum Served {
    /// A new body, parsed before the cache stored it.
    Fresh(Document),
    /// The cached body, still current: the server confirmed it, or asked
    /// us to keep using it for a while.
    Cached(Body),
}

/// Fetch the feed at `url` through `http`, using and updating the `cache`.
/// Every cache decision and every timestamp written uses `now`.
///
/// When the server fails for a reason that may pass, the cached feed is
/// served instead, and [`Fetched::stale_because`] says why.
///
/// # Errors
///
/// Returns the failure when the feed is gone, when it failed and the cache
/// holds nothing to serve, or when a cached body that is still current no
/// longer parses.
async fn fetch_feed(
    url: &Url,
    http: &Http,
    cache: &Cache,
    now: Timestamp,
) -> Result<Fetched, OpenringError> {
    let served = match cache.plan(url, now) {
        Plan::Serve(body) => Ok(Served::Cached(body)),
        Plan::Request(validators) => fetch_from_server(url, http, cache, &validators, now).await,
    };
    let document = match served {
        Ok(Served::Fresh(document)) => document,
        // The server sent no new body, so a cached body that no longer
        // parses leaves nothing to serve. During a Retry window, asking the
        // server anyway would ignore what it asked for.
        Ok(Served::Cached(body)) => parse(&body)?,
        Err(Failure::Gone(e)) => return Err(e),
        // A cached copy that does not parse cannot fill in, and the failure
        // that sent us to the cache is the one worth reporting.
        Err(Failure::Passing(e)) => {
            return match cache.fallback(url).and_then(|body| parse(&body).ok()) {
                Some(document) => Ok(Fetched {
                    document,
                    stale_because: Some(e),
                }),
                None => Err(e),
            };
        }
    };
    Ok(Fetched {
        document,
        stale_because: None,
    })
}

/// Parse a feed body. A body passes this before the cache records it, but
/// a body loaded from disk may not parse, as after a parser upgrade.
fn parse(body: &[u8]) -> Result<Document, OpenringError> {
    Ok(parser::parse(body)?)
}

/// Ask the server for the feed, record its answer in the cache, and say
/// which feed the answer leaves to serve.
async fn fetch_from_server(
    url: &Url,
    http: &Http,
    cache: &Cache,
    validators: &Validators,
    now: Timestamp,
) -> Result<Served, Failure> {
    let passing = Failure::Passing;
    let mut headers = Vec::with_capacity(2);
    if let Some(last_modified) = &validators.last_modified {
        headers.push(("if-modified-since", last_modified.as_str()));
    }
    if let Some(etag) = &validators.etag {
        headers.push(("if-none-match", etag.as_str()));
    }
    debug!(url=%url, ?headers, "sending request");

    let resp = http
        .get(url, &headers)
        .await
        .map_err(|e| passing(e.into()))?;
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
    let outcome = if status == StatusCode::NOT_MODIFIED {
        Outcome::NotModified(response_validators(&resp))
    } else if status.is_success() {
        // Reject grossly oversized bodies before buffering them. Only a
        // success has a body worth reading, so only it is checked: a 404 or
        // 429 that announces a huge body still means what its status says.
        // Chunked and compressed responses announce no usable length, so
        // the read below enforces the same cap as the bytes arrive.
        if let Some(bytes) = resp.content_length()
            && !usize::try_from(bytes).is_ok_and(|b| b <= MAX_FEED_BYTES)
        {
            return Err(passing(OpenringError::FeedTooLargeError {
                url: url.as_str().to_string(),
                bytes,
            }));
        }
        let validators = response_validators(&resp);
        // Keep the raw bytes: pre-decoding to text re-encodes the transfer
        // as UTF-8 while the XML prolog still declares the original charset,
        // so the parser would decode non-UTF-8 feeds twice into mojibake.
        let body = resp
            .read_capped(MAX_FEED_BYTES)
            .await
            .map_err(|e| match e {
                ReadError::TooLarge { bytes } => OpenringError::FeedTooLargeError {
                    url: url.as_str().to_string(),
                    bytes,
                },
                ReadError::Transfer(e) => e.into(),
            })
            .map_err(passing)?;
        // Parse before storing: a body that is empty or not a feed, such as
        // an HTML error page sent with a 200, must not replace a good cached
        // body and its Validators.
        if body.is_empty() {
            return Err(passing(OpenringError::EmptyFeedError(
                url.as_str().to_string(),
            )));
        }
        let document = parse(&body).map_err(passing)?;
        cache
            .record(url, Outcome::Fresh { body, validators }, now)
            .map_err(passing)?;
        return Ok(Served::Fresh(document));
    } else if status == StatusCode::TOO_MANY_REQUESTS {
        Outcome::RateLimited {
            until: retry_deadline(resp.header("retry-after"), now),
        }
    } else {
        let e = OpenringError::UnexpectedStatusError {
            url: url.as_str().to_string(),
            status: status.as_str().to_string(),
        };
        return Err(
            if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
                Failure::Gone(e)
            } else {
                passing(e)
            },
        );
    };
    let body = cache.record(url, outcome, now).map_err(passing)?;
    Ok(Served::Cached(body))
}

#[cfg(test)]
mod tests {
    use hegel::generators;
    use jiff::{SignedDuration, Timestamp, ToSpan};
    use url::Url;

    use super::{DEFAULT_RETRY_WINDOW, MAX_FEED_BYTES, fetch_feed, normalize_etag, retry_deadline};
    use crate::{
        cache::{Cache, Outcome, Plan, Validators},
        error::OpenringError,
        http::{Answer, Http, Request, StatusCode},
        test_support::block_on,
    };

    // A valid RSS 2.0 feed with a parameterized title, for the tests that
    // need a body the parser will accept.
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

    // Whole fetches against a nulled Http and a real cache: the request
    // that goes out, and what each answer means for the next run.

    fn now() -> Timestamp {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }

    fn feed_url() -> Url {
        Url::parse("https://feed.example/feed.xml").unwrap()
    }

    fn validators(etag: Option<&str>, last_modified: Option<&str>) -> Validators {
        Validators {
            etag: etag.map(str::to_string),
            last_modified: last_modified.map(str::to_string),
        }
    }

    // A cache that already holds a feed titled `title` from an earlier run.
    fn cache_holding(title: &str, validators: Validators) -> Cache {
        let cache = Cache::default();
        cache
            .record(
                &feed_url(),
                Outcome::Fresh {
                    body: get_valid_rss_feed(title).into_bytes(),
                    validators,
                },
                now(),
            )
            .unwrap();
        cache
    }

    fn title(fetched: &super::Fetched) -> &str {
        &fetched
            .document
            .title
            .as_ref()
            .expect("feed has a title")
            .content
    }

    // Fetch with a cache that holds a feed titled "cached" with etag v1,
    // against a server that gives `answer`.
    async fn fetch_over_cached(answer: Answer) -> (Result<super::Fetched, OpenringError>, Cache) {
        let cache = cache_holding("cached", validators(Some("\"v1\""), None));
        let http = Http::create_null([(feed_url(), answer)]);
        (fetch_feed(&feed_url(), &http, &cache, now()).await, cache)
    }

    // The cache serves its copy, says why, and keeps its Validators for the
    // next run.
    fn assert_served_stale(
        result: &Result<super::Fetched, OpenringError>,
        cache: &Cache,
        cause: &str,
    ) {
        let fetched = result.as_ref().expect("served the cached feed");
        assert_eq!(title(fetched), "cached");
        assert_eq!(
            fetched
                .stale_because
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some(cause)
        );
        assert_eq!(
            cache.plan(&feed_url(), now()),
            Plan::Request(validators(Some("\"v1\""), None))
        );
    }

    fn request(headers: &[(&'static str, &str)]) -> Request {
        Request {
            url: feed_url(),
            headers: headers
                .iter()
                .map(|&(name, value)| (name, value.to_string()))
                .collect(),
        }
    }

    #[tokio::test]
    async fn sends_validators_and_serves_the_cache_on_304() {
        let cache = cache_holding(
            "cached",
            validators(Some("\"abc123\""), Some("Mon, 01 Jan 2024 00:00:00 GMT")),
        );
        let http = Http::create_null([(feed_url(), Answer::status(304))]);
        let requests = http.track_requests();

        let feed = fetch_feed(&feed_url(), &http, &cache, now())
            .await
            .expect("served cache on 304");
        assert_eq!(title(&feed), "cached");
        assert_eq!(
            requests.data(),
            [request(&[
                ("if-modified-since", "Mon, 01 Jan 2024 00:00:00 GMT"),
                ("if-none-match", "\"abc123\"")
            ])]
        );
    }

    #[tokio::test]
    async fn a_304_rotates_the_validators_for_the_next_run() {
        let cache = cache_holding("cached", validators(Some("\"old\""), None));
        // RFC 7232 allows a 304 to rotate the validators it confirms.
        let http = Http::create_null([(feed_url(), Answer::status(304).header("etag", "\"new\""))]);

        fetch_feed(&feed_url(), &http, &cache, now())
            .await
            .expect("served cache on 304");
        assert_eq!(
            cache.plan(&feed_url(), now()),
            Plan::Request(validators(Some("\"new\""), None))
        );
    }

    #[tokio::test]
    async fn a_304_after_an_expired_retry_window_does_not_reopen_it() {
        let t0 = now();
        let cache = cache_holding("cached", validators(Some("\"v1\""), None));
        cache
            .record(
                &feed_url(),
                Outcome::RateLimited {
                    until: t0 + 1.hour(),
                },
                t0,
            )
            .unwrap();
        let http = Http::create_null([(feed_url(), Answer::status(304))]);
        let requests = http.track_requests();

        for run in [t0 + 2.hours(), t0 + 2.hours() + 1.second()] {
            fetch_feed(&feed_url(), &http, &cache, run)
                .await
                .unwrap_or_else(|e| panic!("run at {run} served cache on 304: {e}"));
        }
        // Two runs, two requests: a 304 ends the rate limit just as a 200
        // does, so the second run must ask the server again.
        assert_eq!(requests.data().len(), 2);
    }

    #[tokio::test]
    async fn a_200_stores_its_validators_for_the_next_run() {
        let http = Http::create_null([(
            feed_url(),
            Answer::ok(get_valid_rss_feed("fresh"))
                .header("etag", "feed-etag")
                .header("last-modified", "Mon, 01 Jan 2024 00:00:00 GMT"),
        )]);
        let cache = Cache::default();

        let feed = fetch_feed(&feed_url(), &http, &cache, now())
            .await
            .expect("fetched fresh feed");
        assert_eq!(title(&feed), "fresh");
        assert_eq!(
            cache.plan(&feed_url(), now()),
            Plan::Request(validators(
                Some("\"feed-etag\""),
                Some("Mon, 01 Jan 2024 00:00:00 GMT")
            ))
        );
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
            br"</title><link>https://x.example/</link><description>d</description></channel></rss>",
        );
        let http = Http::create_null([(feed_url(), vec![Answer::ok(body), Answer::status(304)])]);
        let cache = Cache::default();

        let feed = fetch_feed(&feed_url(), &http, &cache, now())
            .await
            .expect("parsed latin-1 feed");
        assert_eq!(title(&feed), "caf\u{e9}");

        // The cached bytes must reparse identically on a later 304.
        let again = fetch_feed(&feed_url(), &http, &cache, now())
            .await
            .expect("served cache on 304");
        assert_eq!(title(&again), "caf\u{e9}");
    }

    #[tokio::test]
    async fn streamed_bodies_past_the_cap_are_rejected_and_not_cached() {
        // No Content-Length, the shape a chunked or compressed transfer
        // takes. The header check cannot see these; only the read cap can.
        let http = Http::create_null([(
            feed_url(),
            Answer::status(200)
                .chunks(vec![vec![b'x'; MAX_FEED_BYTES], vec![b'x']])
                .chunked(),
        )]);
        let cache = Cache::default();

        let res = fetch_feed(&feed_url(), &http, &cache, now()).await;
        assert!(matches!(
            res,
            Err(OpenringError::FeedTooLargeError { bytes, .. })
                if bytes == u64::try_from(MAX_FEED_BYTES).unwrap() + 1
        ));
        assert_eq!(
            cache.plan(&feed_url(), now()),
            Plan::Request(Validators::default())
        );
    }

    #[tokio::test]
    async fn oversized_feeds_are_rejected_by_their_announced_length() {
        // A urls-file typo that points at a video or other huge file.
        let announced = u64::try_from(MAX_FEED_BYTES).unwrap() + 1;
        let http = Http::create_null([(
            feed_url(),
            Answer::ok(get_valid_rss_feed("huge"))
                .header("etag", "\"huge\"")
                .content_length(announced),
        )]);
        let cache = Cache::default();

        let res = fetch_feed(&feed_url(), &http, &cache, now()).await;
        assert!(matches!(
            res,
            Err(OpenringError::FeedTooLargeError { bytes, .. }) if bytes == announced
        ));
        // Nothing that big belongs in the cache either.
        assert_eq!(
            cache.plan(&feed_url(), now()),
            Plan::Request(Validators::default())
        );
    }

    #[tokio::test]
    async fn a_failed_request_serves_the_cache() {
        let (result, cache) = fetch_over_cached(Answer::send_error("connection refused")).await;
        assert_served_stale(&result, &cache, "connection refused");
    }

    #[tokio::test]
    async fn a_failed_request_with_nothing_cached_is_an_error() {
        let http = Http::create_null([(feed_url(), Answer::send_error("connection refused"))]);
        let res = fetch_feed(&feed_url(), &http, &Cache::default(), now()).await;
        assert!(
            matches!(&res, Err(OpenringError::HttpError(e)) if e.to_string() == "connection refused"),
            "{res:?}"
        );
    }

    #[tokio::test]
    async fn a_body_that_stops_arriving_serves_the_cache() {
        let (result, cache) = fetch_over_cached(
            Answer::ok(get_valid_rss_feed("partial"))
                .header("etag", "\"v2\"")
                .transfer_error("connection reset"),
        )
        .await;
        assert_served_stale(&result, &cache, "connection reset");
    }

    // A 200 that carries an HTML error page instead of a feed must not
    // replace the good copy. If it did, its Validators would go out next
    // run, the server would answer 304, and the error page would be served
    // until it changed.
    #[tokio::test]
    async fn a_200_that_is_not_a_feed_serves_the_cache_and_keeps_it() {
        let (result, cache) = fetch_over_cached(
            Answer::ok("<html><body>Service Unavailable</body></html>").header("etag", "\"v2\""),
        )
        .await;
        assert!(
            matches!(
                result.as_ref().map(|f| f.stale_because.as_ref()),
                Ok(Some(OpenringError::ParseFeedError(_)))
            ),
            "{result:?}"
        );
        let cause = result.as_ref().unwrap().stale_because.as_ref().unwrap();
        assert_served_stale(&result, &cache, &cause.to_string());
    }

    #[tokio::test]
    async fn an_empty_200_serves_the_cache_and_keeps_it() {
        let (result, cache) = fetch_over_cached(Answer::ok("").header("etag", "\"v2\"")).await;
        assert_served_stale(
            &result,
            &cache,
            "The feed at `https://feed.example/feed.xml` was empty.",
        );
    }

    #[tokio::test]
    async fn an_oversized_answer_serves_the_cache() {
        let announced = u64::try_from(MAX_FEED_BYTES).unwrap() + 1;
        let (result, cache) = fetch_over_cached(Answer::ok("big").content_length(announced)).await;
        assert_served_stale(
            &result,
            &cache,
            &format!(
                "The feed at `https://feed.example/feed.xml` is too large ({announced} bytes)."
            ),
        );
    }

    // Any status that is not a feed, a 304, a 429, or a sign the feed is
    // gone may pass by a later run, so the cached copy fills in.
    #[hegel::test]
    fn a_failing_status_serves_the_cache(tc: hegel::TestCase) {
        let code = tc.draw(generators::integers::<u16>().min_value(300).max_value(599));
        tc.assume(![304, 404, 410, 429].contains(&code));
        let (result, cache) = block_on(fetch_over_cached(Answer::status(code)));
        assert_served_stale(
            &result,
            &cache,
            &format!(
                "The request feed at `https://feed.example/feed.xml` received an unexpected error (HTTP {code})."
            ),
        );
    }

    // A 404 or 410 says the feed is gone. Only the user can fix that, so
    // the cached copy must not hide it.
    #[tokio::test]
    async fn a_gone_feed_is_an_error_even_with_a_cached_copy() {
        let huge = u64::try_from(MAX_FEED_BYTES).unwrap() + 1;
        for code in [404, 410] {
            // An announced body too large to read changes nothing: the
            // status alone says the feed is gone.
            for answer in [
                Answer::status(code),
                Answer::status(code).content_length(huge),
            ] {
                let (result, _) = fetch_over_cached(answer).await;
                assert!(
                    matches!(&result, Err(OpenringError::UnexpectedStatusError { status, .. }) if *status == code.to_string()),
                    "{code}: {result:?}"
                );
            }
        }
    }

    // Across the whole Feed list, a gone Feed yields no Source and an Error
    // line, though the cache holds a copy.
    #[tokio::test]
    async fn a_gone_feed_shows_an_error_and_yields_no_source() {
        use crate::{
            feed_list::Feed,
            terminal::{Terminal, Tone},
        };

        let cache = cache_holding("cached", Validators::default());
        let http = Http::create_null([(feed_url(), Answer::status(404))]);
        let terminal = Terminal::create_null(true);
        let lines = terminal.track_lines();
        let feeds = vec![Feed {
            url: feed_url(),
            weight: None,
        }];

        let sources = super::fetch_sources(&http, &terminal, feeds, &cache, now()).await;

        assert!(sources.is_empty(), "{sources:?}");
        let lines = lines.data();
        let [line] = lines.as_slice() else {
            panic!("expected one status line: {lines:?}");
        };
        assert_eq!((line.word, line.tone), ("Error", Tone::Bad));
        assert!(line.detail.starts_with(feed_url().as_str()), "{line:?}");
    }

    // A 429 that announces a huge body is still a rate limit: the cache
    // serves its copy as current and remembers the Retry window.
    #[tokio::test]
    async fn a_429_announcing_a_huge_body_still_opens_a_retry_window() {
        let huge = u64::try_from(MAX_FEED_BYTES).unwrap() + 1;
        let (result, cache) = fetch_over_cached(
            Answer::status(429)
                .header("retry-after", "120")
                .content_length(huge),
        )
        .await;
        let fetched = result.expect("served the cache");
        assert!(fetched.stale_because.is_none());
        assert!(matches!(
            cache.plan(&feed_url(), now() + 119.seconds()),
            Plan::Serve(_)
        ));
    }

    // When the cached copy itself does not parse, it cannot fill in, and
    // the error reported is the server's, not the parser's.
    #[tokio::test]
    async fn an_unparseable_cached_copy_reports_the_original_failure() {
        let cache = Cache::default();
        cache
            .record(
                &feed_url(),
                Outcome::Fresh {
                    body: b"<html>not a feed</html>".to_vec(),
                    validators: Validators::default(),
                },
                now(),
            )
            .unwrap();
        let http = Http::create_null([(feed_url(), Answer::status(503))]);

        let res = fetch_feed(&feed_url(), &http, &cache, now()).await;
        assert!(
            matches!(&res, Err(OpenringError::UnexpectedStatusError { status, .. }) if status == "503"),
            "{res:?}"
        );
    }

    #[tokio::test]
    async fn an_empty_body_is_not_cached_as_servable() {
        // A misbehaving feed: 200 with an etag but a completely empty body.
        let http = Http::create_null([(feed_url(), Answer::ok("").header("etag", "\"empty\""))]);
        let requests = http.track_requests();
        let cache = Cache::default();

        let first = fetch_feed(&feed_url(), &http, &cache, now()).await;
        assert!(matches!(first, Err(OpenringError::EmptyFeedError(_))));

        // The empty body must not be cached. If it were, the next fetch
        // would send If-None-Match, the server would say 304, and the run
        // would serve the empty body until the entry aged out of the cache.
        let second = fetch_feed(&feed_url(), &http, &cache, now()).await;
        assert!(matches!(second, Err(OpenringError::EmptyFeedError(_))));
        assert_eq!(
            requests.data(),
            [request(&[]), request(&[])],
            "second fetch sent a conditional request with nothing to serve"
        );
    }

    #[tokio::test]
    async fn a_429_serves_the_cache_until_its_retry_after_passes() {
        let cache = cache_holding("rate-limited", Validators::default());
        let http =
            Http::create_null([(feed_url(), Answer::status(429).header("retry-after", "120"))]);

        let feed = fetch_feed(&feed_url(), &http, &cache, now())
            .await
            .expect("served cache on 429");
        assert_eq!(title(&feed), "rate-limited");
        assert!(matches!(
            cache.plan(&feed_url(), now() + 119.seconds()),
            Plan::Serve(_)
        ));
        assert!(matches!(
            cache.plan(&feed_url(), now() + 120.seconds()),
            Plan::Request(_)
        ));
    }

    #[tokio::test]
    async fn a_429_with_an_http_date_waits_until_that_date() {
        // RFC 9110 allows Retry-After to be an HTTP-date as well as seconds.
        let until = now() + 600.seconds();
        let header =
            jiff::fmt::rfc2822::to_string(&until.to_zoned(jiff::tz::TimeZone::UTC)).unwrap();
        let cache = cache_holding("rate-limited", Validators::default());
        let http = Http::create_null([(
            feed_url(),
            Answer::status(429).header("retry-after", &header),
        )]);

        fetch_feed(&feed_url(), &http, &cache, now())
            .await
            .expect("served cache on 429");
        assert!(matches!(
            cache.plan(&feed_url(), until - SignedDuration::from_secs(1)),
            Plan::Serve(_)
        ));
        assert!(matches!(cache.plan(&feed_url(), until), Plan::Request(_)));
    }

    #[tokio::test]
    async fn an_open_retry_window_serves_the_cache_without_a_request() {
        let cache = cache_holding("cached", Validators::default());
        cache
            .record(
                &feed_url(),
                Outcome::RateLimited {
                    until: now() + 1.hour(),
                },
                now(),
            )
            .unwrap();
        let http = Http::create_null::<Answer>([]);
        let requests = http.track_requests();

        let feed = fetch_feed(&feed_url(), &http, &cache, now() + 30.minutes())
            .await
            .expect("served the cache");
        assert_eq!(title(&feed), "cached");
        assert_eq!(requests.data(), []);
    }

    // A cached body that no longer parses, as after a parser upgrade, fails
    // the fetch during a Retry window. The server asked not to be asked, so
    // no request goes out.
    #[tokio::test]
    async fn an_open_retry_window_over_an_unparseable_body_fails_without_a_request() {
        let cache = Cache::default();
        let body = b"not a feed".to_vec();
        let fresh = Outcome::Fresh {
            body,
            validators: Validators::default(),
        };
        cache.record(&feed_url(), fresh, now()).unwrap();
        let until = now() + 1.hour();
        cache
            .record(&feed_url(), Outcome::RateLimited { until }, now())
            .unwrap();
        let http = Http::create_null::<Answer>([]);
        let requests = http.track_requests();

        let res = fetch_feed(&feed_url(), &http, &cache, now() + 30.minutes()).await;
        assert!(
            matches!(res, Err(OpenringError::ParseFeedError(_))),
            "{res:?}"
        );
        assert_eq!(requests.data(), []);
    }

    #[tokio::test]
    async fn a_429_for_an_unknown_feed_is_a_rate_limit_error() {
        let http = Http::create_null([(feed_url(), Answer::status(429))]);
        let res = fetch_feed(&feed_url(), &http, &Cache::default(), now()).await;
        assert!(matches!(res, Err(OpenringError::RateLimitError(_))));
    }

    #[tokio::test]
    async fn a_redirected_feed_stays_cached_under_the_requested_url() {
        let moved = Url::parse("https://feed.example/new.xml").unwrap();
        let http = Http::create_null([(
            feed_url(),
            Answer::ok(get_valid_rss_feed("moved"))
                .header("etag", "\"v1\"")
                .redirected_to(moved.clone()),
        )]);
        let cache = Cache::default();

        let feed = fetch_feed(&feed_url(), &http, &cache, now())
            .await
            .expect("followed the redirect");
        assert_eq!(title(&feed), "moved");
        // The cache stays keyed by the URL the user configured, so later runs
        // still find their entry.
        assert_eq!(
            cache.plan(&feed_url(), now()),
            Plan::Request(validators(Some("\"v1\""), None))
        );
        assert_eq!(
            cache.plan(&moved, now()),
            Plan::Request(Validators::default())
        );
    }

    #[tokio::test]
    async fn unexpected_status_is_an_error() {
        let http = Http::create_null([(feed_url(), Answer::status(500))]);
        let res = fetch_feed(&feed_url(), &http, &Cache::default(), now()).await;
        assert!(matches!(
            res,
            Err(OpenringError::UnexpectedStatusError { status, .. })
                if status == StatusCode::INTERNAL_SERVER_ERROR.as_str()
        ));
    }
}
