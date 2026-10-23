//! The feed fetch module: HTTP for one Feed. It asks the cache what to do,
//! sends the request, turns the response into an [`Outcome`], and parses
//! the body the cache hands back. Every rule about what the cache stores
//! lives in the cache module.

use feed_rs::{model::Feed, parser};
use jiff::{SignedDuration, Timestamp};
use tracing::{debug, warn};
use url::Url;

use crate::{
    cache::{Cache, Outcome, Plan, Validators},
    error::OpenringError,
    http::{Http, ReadError, Response, StatusCode},
};

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

/// Fetch the feed at `url` through `http`, using and updating the `cache`.
/// Every cache decision and every timestamp written uses `now`.
pub(crate) async fn fetch_feed(
    url: &Url,
    http: &Http,
    cache: &Cache,
    now: Timestamp,
) -> Result<Feed, OpenringError> {
    let validators = match cache.plan(url, now) {
        Plan::Serve(body) => return Ok(parser::parse(body.as_slice())?),
        Plan::Request(validators) => validators,
    };
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

    // Reject grossly oversized responses before buffering them. Chunked and
    // compressed responses announce no usable length, so the read below
    // enforces the same cap as the bytes arrive.
    if let Some(bytes) = resp.content_length()
        && !usize::try_from(bytes).is_ok_and(|b| b <= MAX_FEED_BYTES)
    {
        return Err(OpenringError::FeedTooLargeError {
            url: url.as_str().to_string(),
            bytes,
        });
    }

    let status = resp.status();
    let outcome = if status == StatusCode::NOT_MODIFIED {
        Outcome::NotModified(response_validators(&resp))
    } else if status.is_success() {
        let validators = response_validators(&resp);
        // Keep the raw bytes: pre-decoding to text re-encodes the transfer
        // as UTF-8 while the XML prolog still declares the original charset,
        // so the parser would decode non-UTF-8 feeds twice into mojibake. A
        // transfer error mid-read fails the fetch with its cause and leaves
        // the cache untouched.
        let body = resp
            .read_capped(MAX_FEED_BYTES)
            .await
            .map_err(|e| match e {
                ReadError::TooLarge { bytes } => OpenringError::FeedTooLargeError {
                    url: url.as_str().to_string(),
                    bytes,
                },
                ReadError::Transfer(e) => e.into(),
            })?;
        Outcome::Fresh { body, validators }
    } else if status == StatusCode::TOO_MANY_REQUESTS {
        Outcome::RateLimited {
            until: retry_deadline(resp.header("retry-after"), now),
        }
    } else {
        return Err(OpenringError::UnexpectedStatusError {
            url: url.as_str().to_string(),
            status: status.as_str().to_string(),
        });
    };
    let body = cache.record(url, outcome, now)?;
    Ok(parser::parse(body.as_slice())?)
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

    // Retry-After parsing.

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

    const NOW: &str = "2026-01-01T00:00:00Z";

    fn now() -> Timestamp {
        NOW.parse().unwrap()
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
    fn cache_holding(title: &str, validators: Validators, at: Timestamp) -> Cache {
        let cache = Cache::default();
        cache
            .record(
                &feed_url(),
                Outcome::Fresh {
                    body: get_valid_rss_feed(title).into_bytes(),
                    validators,
                },
                at,
            )
            .unwrap();
        cache
    }

    fn title(feed: &feed_rs::model::Feed) -> &str {
        &feed.title.as_ref().expect("feed has a title").content
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
            now(),
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
        let cache = cache_holding("cached", validators(Some("\"old\""), None), now());
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
        let cache = cache_holding("cached", validators(Some("\"v1\""), None), t0);
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
    async fn a_failed_request_is_an_error_and_leaves_the_cache_alone() {
        let cache = cache_holding("cached", validators(Some("\"v1\""), None), now());
        let http = Http::create_null([(feed_url(), Answer::send_error("connection refused"))]);

        let res = fetch_feed(&feed_url(), &http, &cache, now()).await;
        assert!(
            matches!(&res, Err(OpenringError::HttpError(e)) if e.to_string() == "connection refused"),
            "{res:?}"
        );
        assert_eq!(
            cache.plan(&feed_url(), now()),
            Plan::Request(validators(Some("\"v1\""), None))
        );
    }

    #[tokio::test]
    async fn a_body_that_stops_arriving_is_an_error_and_leaves_the_cache_alone() {
        let cache = cache_holding("cached", validators(Some("\"v1\""), None), now());
        let http = Http::create_null([(
            feed_url(),
            Answer::ok(get_valid_rss_feed("partial"))
                .header("etag", "\"v2\"")
                .transfer_error("connection reset"),
        )]);

        let res = fetch_feed(&feed_url(), &http, &cache, now()).await;
        assert!(
            matches!(&res, Err(OpenringError::HttpError(e)) if e.to_string() == "connection reset"),
            "{res:?}"
        );
        assert_eq!(
            cache.plan(&feed_url(), now()),
            Plan::Request(validators(Some("\"v1\""), None))
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

        // The cached entry must not pretend to have something to serve. If it
        // did, the next fetch would send If-None-Match, the server would say
        // 304, and the run would serve the empty body until the entry aged
        // out of the cache.
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
        let cache = cache_holding("rate-limited", Validators::default(), now());
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
        let cache = cache_holding("rate-limited", Validators::default(), now());
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
        let cache = cache_holding("cached", Validators::default(), now());
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
