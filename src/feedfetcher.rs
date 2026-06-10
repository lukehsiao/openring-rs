use std::{sync::Arc, time::Duration};

use clap::{crate_name, crate_version};
use feed_rs::{model::Feed, parser};
use jiff::Timestamp;
use reqwest::{
    StatusCode, {Client, ClientBuilder},
};
use tracing::{debug, warn};
use url::Url;

use crate::{
    cache::{Cache, CacheValue},
    error::OpenringError,
};

pub(crate) trait FeedFetcher {
    /// Fetch a feed using the shared HTTP `client`.
    async fn fetch_feed(&self, client: &Client, cache: &Arc<Cache>) -> Result<Feed, OpenringError>;
}

/// Build the HTTP client shared by every feed fetch: one connection pool and
/// one TLS setup for the whole run, a 30s timeout, and the openring
/// user agent.
pub(crate) fn build_client() -> Result<Client, OpenringError> {
    Ok(ClientBuilder::new()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!(crate_name!(), '/', crate_version!()))
        .build()?)
}

/// Normalize an etag so it carries the literal double quotes HTTP requires.
#[must_use]
pub fn normalize_etag(s: &str) -> String {
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with("W/\"") && s.ends_with('"')) {
        s.to_string()
    } else {
        // ETag values must have the actual quotes
        format!("\"{s}\"")
    }
}

/// Pure decision logic for fetching, separated from all HTTP and cache I/O so it
/// can be exercised directly. Each function takes plain values and returns a plain
/// decision; the coordinator (`fetch_feed`) performs the I/O around it.
pub(crate) mod logic {
    use jiff::{Span, Timestamp, ToSpan};
    use reqwest::StatusCode;

    use crate::cache::{CacheValue, MAX_SPAN_SEC};

    /// The conditional-request headers to attach to a fetch, derived from cache.
    #[derive(Debug, Default, Clone, PartialEq, Eq)]
    pub(crate) struct ConditionalHeaders {
        pub if_modified_since: Option<String>,
        pub if_none_match: Option<String>,
    }

    /// What to do with the cache and response after a fetch, decided purely from
    /// the response. `Reuse` and `RateLimited` direct the coordinator to serve the
    /// body from the live cache entry rather than carrying it in the variant,
    /// since that body can be large and is owned by the map.
    #[derive(Debug, Clone)]
    pub(crate) enum Disposition {
        /// 2xx: overwrite the cached metadata and body, clearing any retry window.
        Store {
            etag: Option<String>,
            last_modified: Option<String>,
            body: Option<String>,
        },
        /// 304 with an existing entry: keep the cached body and metadata.
        Reuse,
        /// 429 with an existing entry: record the retry window and serve cache.
        RateLimited { retry_after: Span },
        /// 429 with no cached entry: there is nothing to serve.
        RateLimitedNoCache,
        /// Any other status code.
        Unexpected { status: String },
    }

    /// Whether a cached 429 retry window is still open at `now`. While it is, the
    /// caller should serve from cache instead of issuing a request.
    pub(crate) fn retry_after_gate_open(cv: &CacheValue, now: Timestamp) -> bool {
        cv.retry_after
            .is_some_and(|retry| cv.timestamp + retry > now)
    }

    /// The conditional-request headers implied by a cached entry, if any.
    pub(crate) fn conditional_headers(cv: Option<&CacheValue>) -> ConditionalHeaders {
        cv.map_or_else(ConditionalHeaders::default, |cv| ConditionalHeaders {
            if_modified_since: cv.last_modified.clone(),
            if_none_match: cv.etag.clone(),
        })
    }

    /// Parse a `Retry-After` header into a `Span`, defaulting to 4 hours when it is
    /// absent or not an integer count of seconds. The value is clamped to the span
    /// jiff can represent (`MAX_SPAN_SEC`) so a hostile header cannot panic a fetch.
    pub(crate) fn parse_retry_after(header: Option<&str>) -> Span {
        header.and_then(|s| s.parse::<i64>().ok()).map_or_else(
            || 4.hours(),
            |secs| secs.clamp(-MAX_SPAN_SEC, MAX_SPAN_SEC).seconds(),
        )
    }

    /// Decide what to do with a response given its status, the already-normalized
    /// `etag` and `last_modified` headers, the body text, whether a cache entry
    /// already exists, and the raw `retry-after` header.
    pub(crate) fn disposition(
        status: StatusCode,
        etag: Option<&str>,
        last_modified: Option<&str>,
        body: Option<String>,
        had_cache_entry: bool,
        retry_after_header: Option<&str>,
    ) -> Disposition {
        if status == StatusCode::NOT_MODIFIED && had_cache_entry {
            Disposition::Reuse
        } else if status.is_success() || status == StatusCode::NOT_MODIFIED {
            // A 2xx, or a 304 with no prior entry, stores the response as-is.
            Disposition::Store {
                etag: etag.map(ToString::to_string),
                last_modified: last_modified.map(ToString::to_string),
                body,
            }
        } else if status == StatusCode::TOO_MANY_REQUESTS {
            if had_cache_entry {
                Disposition::RateLimited {
                    retry_after: parse_retry_after(retry_after_header),
                }
            } else {
                Disposition::RateLimitedNoCache
            }
        } else {
            Disposition::Unexpected {
                status: status.as_str().to_string(),
            }
        }
    }
}

/// Parse feed bytes, logging and converting any parse error.
fn parse_feed(url: &Url, feed_str: &str) -> Result<Feed, OpenringError> {
    parser::parse(feed_str.as_bytes()).map_err(|e| {
        warn!(url=%url.as_str(), error=%e, "failed to parse feed.");
        OpenringError::from(e)
    })
}

/// Apply a decided [`logic::Disposition`] to the cache and return the feed body to
/// serve, or a terminal error. This is the write half of a fetch; the decision is
/// made purely in [`logic::disposition`].
fn apply_disposition(
    url: &Url,
    cache: &Cache,
    now: Timestamp,
    disposition: logic::Disposition,
) -> Result<String, OpenringError> {
    match disposition {
        logic::Disposition::Store {
            etag,
            last_modified,
            body,
        } => {
            if let Some(mut cv) = cache.get_mut(url) {
                cv.etag = etag;
                cv.last_modified = last_modified;
                cv.body.clone_from(&body);
                cv.timestamp = now;
                // A fresh success invalidates any stale 429 retry window.
                cv.retry_after = None;
            } else {
                cache.insert(
                    url.clone(),
                    CacheValue {
                        timestamp: now,
                        retry_after: None,
                        etag,
                        last_modified,
                        body: body.clone(),
                    },
                );
            }
            body.ok_or_else(|| OpenringError::EmptyFeedError(url.as_str().to_string()))
        }
        logic::Disposition::Reuse => cache
            .get_mut(url)
            .and_then(|mut cv| {
                cv.timestamp = now;
                cv.body.clone()
            })
            .ok_or_else(|| OpenringError::EmptyFeedError(url.as_str().to_string())),
        logic::Disposition::RateLimited { retry_after } => cache
            .get_mut(url)
            .and_then(|mut cv| {
                cv.timestamp = now;
                cv.retry_after = Some(retry_after);
                cv.body.clone()
            })
            .ok_or_else(|| OpenringError::EmptyFeedError(url.as_str().to_string())),
        logic::Disposition::RateLimitedNoCache => {
            Err(OpenringError::RateLimitError(url.as_str().to_string()))
        }
        logic::Disposition::Unexpected { status } => Err(OpenringError::UnexpectedStatusError {
            url: url.as_str().to_string(),
            status,
        }),
    }
}

impl FeedFetcher for Url {
    /// Fetch a feed for a URL
    async fn fetch_feed(&self, client: &Client, cache: &Arc<Cache>) -> Result<Feed, OpenringError> {
        // Capture the clock once so every timestamp written during this call agrees
        // and so the decision logic can be exercised deterministically.
        let now = Timestamp::now();

        // Snapshot the entry by value so no DashMap guard is held across an await
        // point; concurrent fetches share the map through a JoinSet.
        let cached: Option<CacheValue> = cache.get(self).map(|e| e.value().clone());

        // While a 429 retry window is open, serve the cached feed without a request.
        // An open window with no cached body falls through and fetches.
        if let Some(cv) = &cached
            && logic::retry_after_gate_open(cv, now)
        {
            debug!(timestamp=%cv.timestamp, retry_after=?cv.retry_after, "skipping request due to 429, using feed from cache");
            if let Some(feed_str) = &cv.body {
                return parse_feed(self, feed_str);
            }
            warn!(url=%self.as_str(), "empty feed");
        }

        let mut req = client.get(self.as_str());
        let headers = logic::conditional_headers(cached.as_ref());
        if let Some(last_modified) = &headers.if_modified_since {
            req = req.header("If-Modified-Since", last_modified);
        }
        if let Some(etag) = &headers.if_none_match {
            req = req.header("If-None-Match", etag);
        }
        debug!(url=%self, request=?req, "sending request");

        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(e) => {
                warn!(url=%self.as_str(), error=%e, "failed to get feed.");
                return Err(e.into());
            }
        };
        debug!(url=%self, response=?resp, "received response");

        // Pull the plain values the decision logic needs out of the response before
        // `text()` consumes it. The etag is normalized at this boundary.
        let status = resp.status();
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(normalize_etag);
        let last_modified = resp
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = if status.is_success() || status == StatusCode::NOT_MODIFIED {
            resp.text().await.ok()
        } else {
            None
        };

        let disposition = logic::disposition(
            status,
            etag.as_deref(),
            last_modified.as_deref(),
            body,
            cached.is_some(),
            retry_after.as_deref(),
        );
        let feed_str = apply_disposition(self, cache, now, disposition)?;
        parse_feed(self, &feed_str)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use jiff::{Span, Timestamp, ToSpan};
    use reqwest::StatusCode;
    use url::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use hegel::extras::jiff as jiff_gs;
    use hegel::generators;

    use crate::cache::{Cache, CacheValue, MAX_SPAN_SEC};
    use crate::error::OpenringError;

    use super::{FeedFetcher, build_client, logic, normalize_etag};

    // Bounds for gate timestamps/spans. 50e9 seconds is ~1585 years past the
    // epoch; a timestamp plus a span stays under jiff's Timestamp::MAX, while
    // `now` (up to 200e9s) can still fall on either side of any deadline.
    const GATE_SECONDS_MAX: i64 = 50_000_000_000;
    const GATE_NOW_MAX: i64 = 200_000_000_000;

    // A valid RSS 2.0 feed with a parameterized title, for the HTTP integration
    // tests that need a body the parser will accept.
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

    // retry_after only ever holds a time-only span (seconds, from parse_retry_after),
    // so we generate seconds rather than jiff_gs::spans(), whose calendar-unit spans
    // are outside this domain and would trip cache::spans_equal.
    #[hegel::composite]
    fn retry_spans(tc: hegel::TestCase) -> Span {
        let secs = tc.draw(generators::integers::<i64>());
        // Clamp to the span jiff can represent (see cache::MAX_SPAN_SEC).
        Span::new().seconds(secs.clamp(0, MAX_SPAN_SEC))
    }

    #[hegel::composite]
    fn cache_values(tc: hegel::TestCase) -> CacheValue {
        CacheValue {
            timestamp: tc.draw(jiff_gs::timestamps()),
            retry_after: tc.draw(generators::optional(retry_spans())),
            last_modified: tc.draw(generators::optional(generators::text())),
            etag: tc.draw(generators::optional(generators::text())),
            body: tc.draw(generators::optional(generators::text())),
        }
    }

    // `parse_retry_after` accepts any header value without panicking.
    #[hegel::test]
    fn parse_retry_after_never_panics(tc: hegel::TestCase) {
        let header = tc.draw(generators::optional(generators::text()));
        let _ = logic::parse_retry_after(header.as_deref());
    }

    // An integer header yields that many seconds, clamped to jiff's span range.
    #[hegel::test]
    fn parse_retry_after_clamps_integer_seconds(tc: hegel::TestCase) {
        let secs = tc.draw(generators::integers::<i64>());
        let span = logic::parse_retry_after(Some(&secs.to_string()));
        assert_eq!(span.get_seconds(), secs.clamp(-MAX_SPAN_SEC, MAX_SPAN_SEC));
    }

    // Anything that is not an integer count of seconds falls back to 4 hours.
    #[hegel::test]
    fn parse_retry_after_defaults_when_not_seconds(tc: hegel::TestCase) {
        let header = tc.draw(generators::optional(generators::text()));
        tc.assume(
            header
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
                .is_none(),
        );
        let span = logic::parse_retry_after(header.as_deref());
        assert_eq!(span.fieldwise(), 4.hours().fieldwise());
    }

    // The gate is open exactly when the retry deadline is still in the future.
    // An independent integer-seconds computation is the oracle for jiff's
    // Timestamp + Span arithmetic.
    #[hegel::test]
    fn gate_open_iff_deadline_after_now(tc: hegel::TestCase) {
        let ts_secs = tc.draw(
            generators::integers::<i64>()
                .min_value(0)
                .max_value(GATE_SECONDS_MAX),
        );
        let retry_secs = tc.draw(
            generators::integers::<i64>()
                .min_value(0)
                .max_value(GATE_SECONDS_MAX),
        );
        let now_secs = tc.draw(
            generators::integers::<i64>()
                .min_value(0)
                .max_value(GATE_NOW_MAX),
        );

        let cv = CacheValue {
            timestamp: Timestamp::from_second(ts_secs).unwrap(),
            retry_after: Some(Span::new().seconds(retry_secs)),
            last_modified: None,
            etag: None,
            body: None,
        };
        let now = Timestamp::from_second(now_secs).unwrap();

        // Both bounded under GATE_SECONDS_MAX, so the i64 sum cannot overflow.
        let deadline_secs = ts_secs + retry_secs;
        assert_eq!(
            logic::retry_after_gate_open(&cv, now),
            now_secs < deadline_secs
        );
    }

    // Without a retry_after window the gate is always closed.
    #[hegel::test]
    fn gate_closed_without_retry_after(tc: hegel::TestCase) {
        let mut cv = tc.draw(cache_values());
        cv.retry_after = None;
        let now = tc.draw(jiff_gs::timestamps());
        assert!(!logic::retry_after_gate_open(&cv, now));
    }

    // Conditional headers are exactly the cached etag and last-modified values.
    #[hegel::test]
    fn conditional_headers_project_cache_fields(tc: hegel::TestCase) {
        let cv = tc.draw(generators::optional(cache_values()));
        let headers = logic::conditional_headers(cv.as_ref());
        assert_eq!(
            headers.if_modified_since,
            cv.as_ref().and_then(|c| c.last_modified.clone())
        );
        assert_eq!(
            headers.if_none_match,
            cv.as_ref().and_then(|c| c.etag.clone())
        );
    }

    // A 2xx response stores the response metadata and body verbatim.
    #[hegel::test]
    fn disposition_success_stores_response(tc: hegel::TestCase) {
        let code = tc.draw(generators::integers::<u16>().min_value(200).max_value(299));
        let status = StatusCode::from_u16(code).expect("2xx is valid");
        let etag = tc.draw(generators::optional(generators::text()));
        let last_modified = tc.draw(generators::optional(generators::text()));
        let body = tc.draw(generators::optional(generators::text()));
        let had_cache_entry = tc.draw(generators::booleans());

        let disp = logic::disposition(
            status,
            etag.as_deref(),
            last_modified.as_deref(),
            body.clone(),
            had_cache_entry,
            None,
        );
        let logic::Disposition::Store {
            etag: e,
            last_modified: lm,
            body: b,
        } = disp
        else {
            panic!("expected Store, got {disp:?}");
        };
        assert_eq!(e, etag);
        assert_eq!(lm, last_modified);
        assert_eq!(b, body);
    }

    // A 304 with a cache entry reuses it; with no entry it stores the response.
    #[hegel::test]
    fn disposition_not_modified_depends_on_cache(tc: hegel::TestCase) {
        let body = tc.draw(generators::optional(generators::text()));
        let had_cache_entry = tc.draw(generators::booleans());
        let disp = logic::disposition(
            StatusCode::NOT_MODIFIED,
            None,
            None,
            body.clone(),
            had_cache_entry,
            None,
        );
        if had_cache_entry {
            assert!(matches!(disp, logic::Disposition::Reuse));
        } else {
            let logic::Disposition::Store { body: b, .. } = disp else {
                panic!("expected Store, got {disp:?}");
            };
            assert_eq!(b, body);
        }
    }

    // A 429 with a cache entry records a retry window matching the standalone
    // parser; with no entry it is a terminal rate-limit decision.
    #[hegel::test]
    fn disposition_too_many_requests_depends_on_cache(tc: hegel::TestCase) {
        let retry_after = tc.draw(generators::optional(generators::text()));
        let had_cache_entry = tc.draw(generators::booleans());
        let disp = logic::disposition(
            StatusCode::TOO_MANY_REQUESTS,
            None,
            None,
            None,
            had_cache_entry,
            retry_after.as_deref(),
        );
        if had_cache_entry {
            let logic::Disposition::RateLimited { retry_after: span } = disp else {
                panic!("expected RateLimited, got {disp:?}");
            };
            assert_eq!(
                span.fieldwise(),
                logic::parse_retry_after(retry_after.as_deref()).fieldwise()
            );
        } else {
            assert!(matches!(disp, logic::Disposition::RateLimitedNoCache));
        }
    }

    // Any status that is not 2xx, 304, or 429 is an unexpected-status decision
    // carrying the numeric status code.
    #[hegel::test]
    fn disposition_other_status_is_unexpected(tc: hegel::TestCase) {
        let code = tc.draw(generators::integers::<u16>().min_value(100).max_value(599));
        tc.assume(!(200..=299).contains(&code) && code != 304 && code != 429);
        let status = StatusCode::from_u16(code).expect("100..=599 is valid");
        let had_cache_entry = tc.draw(generators::booleans());

        let disp = logic::disposition(status, None, None, None, had_cache_entry, None);
        let logic::Disposition::Unexpected { status: s } = disp else {
            panic!("expected Unexpected, got {disp:?}");
        };
        assert_eq!(s, status.as_str());
    }

    // The remaining tests exercise the HTTP wiring that pure tests cannot see:
    // header spelling on the wire, reading a real reqwest::Response, and the
    // status -> cache-mutation path end to end.

    #[tokio::test]
    async fn sends_conditional_headers_and_reuses_on_304() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;

        let url = Url::parse(&server.uri()).unwrap();
        let cache = Arc::new(Cache::new());
        let etag = normalize_etag("abc123");
        let last_modified = "Mon, 01 Jan 2024 00:00:00 GMT".to_string();
        cache.insert(
            url.clone(),
            CacheValue {
                timestamp: Timestamp::now(),
                retry_after: None,
                last_modified: Some(last_modified.clone()),
                etag: Some(etag.clone()),
                body: Some(get_valid_rss_feed("cached")),
            },
        );

        let feed = url
            .fetch_feed(&build_client().unwrap(), &cache)
            .await
            .expect("served cache on 304");
        assert!(
            feed.title
                .as_ref()
                .is_some_and(|t| t.content.contains("cached"))
        );

        let received = server.received_requests().await.unwrap();
        let req = &received[0];
        assert_eq!(
            req.headers.get("if-none-match").unwrap().to_str().unwrap(),
            etag
        );
        assert_eq!(
            req.headers
                .get("if-modified-since")
                .unwrap()
                .to_str()
                .unwrap(),
            last_modified
        );
    }

    #[tokio::test]
    async fn stores_etag_and_last_modified_on_200() {
        let server = MockServer::start().await;
        let etag_raw = "feed-etag";
        let last_modified = "Mon, 01 Jan 2024 00:00:00 GMT";
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("etag", etag_raw)
                    .append_header("last-modified", last_modified)
                    .set_body_string(get_valid_rss_feed("fresh")),
            )
            .mount(&server)
            .await;

        let url = Url::parse(&server.uri()).unwrap();
        let cache = Arc::new(Cache::new());

        let feed = url
            .fetch_feed(&build_client().unwrap(), &cache)
            .await
            .expect("fetched fresh feed");
        assert!(
            feed.title
                .as_ref()
                .is_some_and(|t| t.content.contains("fresh"))
        );

        let entry = cache.get(&url).expect("cached after 200");
        assert_eq!(
            entry.etag.as_deref(),
            Some(normalize_etag(etag_raw).as_str())
        );
        assert_eq!(entry.last_modified.as_deref(), Some(last_modified));
    }

    #[tokio::test]
    async fn rate_limited_serves_cache_and_records_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
            .mount(&server)
            .await;

        let url = Url::parse(&server.uri()).unwrap();
        let cache = Arc::new(Cache::new());
        cache.insert(
            url.clone(),
            CacheValue {
                timestamp: Timestamp::now(),
                retry_after: None,
                last_modified: None,
                etag: None,
                body: Some(get_valid_rss_feed("rate-limited")),
            },
        );

        let feed = url
            .fetch_feed(&build_client().unwrap(), &cache)
            .await
            .expect("served cache on 429");
        assert!(
            feed.title
                .as_ref()
                .is_some_and(|t| t.content.contains("rate-limited"))
        );
        let entry = cache.get(&url).expect("entry present");
        assert_eq!(entry.retry_after.unwrap().get_seconds(), 120);
    }

    #[tokio::test]
    async fn sends_openring_user_agent() {
        use clap::{crate_name, crate_version};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(get_valid_rss_feed("ua")))
            .mount(&server)
            .await;

        let url = Url::parse(&server.uri()).unwrap();
        let cache = Arc::new(Cache::new());
        url.fetch_feed(&build_client().unwrap(), &cache)
            .await
            .expect("fetched");

        let received = server.received_requests().await.unwrap();
        assert_eq!(
            received[0].headers.get("user-agent").unwrap(),
            concat!(crate_name!(), '/', crate_version!())
        );
    }

    #[tokio::test]
    async fn unexpected_status_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let url = Url::parse(&server.uri()).unwrap();
        let cache = Arc::new(Cache::new());
        let res = url.fetch_feed(&build_client().unwrap(), &cache).await;
        assert!(matches!(
            res,
            Err(OpenringError::UnexpectedStatusError { .. })
        ));
    }
}
