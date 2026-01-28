#![allow(clippy::too_many_lines)]

use std::{sync::Arc, time::Duration};

use clap::{crate_name, crate_version};
use feed_rs::{model::Feed, parser};
use jiff::{Timestamp, ToSpan};
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
    /// Fetch a feed
    async fn fetch_feed(&self, cache: &Arc<Cache>) -> Result<Feed, OpenringError>;
}

impl FeedFetcher for Url {
    /// Fetch a feed for a URL
    async fn fetch_feed(&self, cache: &Arc<Cache>) -> Result<Feed, OpenringError> {
        let client: Client = ClientBuilder::new()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!(crate_name!(), '/', crate_version!()))
            .build()?;

        let req = {
            let cache_value = cache.get(self);

            // Respect Retry-After Header if set in cache
            if let Some(ref cv) = cache_value
                && let Some(retry) = cv.retry_after
                && cv.timestamp + retry > Timestamp::now()
            {
                debug!(timestamp=%cv.timestamp, retry_after=%retry, "skipping request due to 429, using feed from cache");

                // TODO: This is just copy-pasted, should be reused
                if let Some(ref feed_str) = cv.body {
                    return match parser::parse(feed_str.as_bytes()) {
                        Ok(feed) => Ok(feed),
                        Err(e) => {
                            warn!(
                                url=%self.as_str(),
                                error=%e,
                                "failed to parse feed."
                            );
                            Err(e.into())
                        }
                    };
                }
                warn!(url = %self.as_str(), "empty feed");
            }

            let mut r = client.get(self.as_str());
            // Add friendly headers if cache is available
            if let Some(ref cv) = cache_value {
                if let Some(last_modified) = &cv.last_modified {
                    r = r.header("If-Modified-Since", last_modified);
                }
                if let Some(etag) = &cv.etag {
                    r = r.header("If-None-Match", etag);
                }
            }
            debug!(url=%self, request=?r, "sending request");
            r
        };

        let body = match req.send().await {
            Ok(r) => {
                debug!(url=%self, response=?r, "received response");
                match r.status() {
                    s if s.is_success() || s == StatusCode::NOT_MODIFIED => {
                        // ETag values must have the actual quotes
                        let etag = r.headers().get("etag").and_then(|etag_value| {
                            // Convert header to str
                            etag_value.to_str().ok().map(|etag_str| {
                                if (etag_str.starts_with('"') && etag_str.ends_with('"'))
                                    || (etag_str.starts_with("W/\"") && etag_str.ends_with('"'))
                                {
                                    etag_str.to_string()
                                } else {
                                    format!("\"{etag_str}\"")
                                }
                            })
                        });
                        let last_modified = r.headers().get("last-modified").and_then(|lm_value| {
                            lm_value.to_str().ok().map(std::string::ToString::to_string)
                        });
                        let status = r.status();
                        let mut body = r.text().await.ok();

                        // Update cache
                        {
                            let cache_value = cache.get_mut(self);
                            if let Some(mut cv) = cache_value {
                                if status == StatusCode::NOT_MODIFIED {
                                    debug!(url=%self, status=status.as_str(), "got 304, using feed from cache");
                                    body.clone_from(&cv.body);
                                } else {
                                    debug!(url=%self, status=status.as_str(), "cache hit, using feed from body");
                                    cv.etag = etag;
                                    cv.last_modified = last_modified;
                                    cv.body.clone_from(&body);
                                }
                                cv.timestamp = Timestamp::now();
                            } else {
                                debug!(url=%self, status=status.as_str(), "using feed from body and adding to cache");
                                cache.insert(
                                    self.clone(),
                                    CacheValue {
                                        timestamp: Timestamp::now(),
                                        retry_after: None,
                                        etag,
                                        last_modified,
                                        body: body.clone(),
                                    },
                                );
                            }
                        }
                        body.ok_or(OpenringError::EmptyFeedError(self.as_str().to_string()))
                    }
                    StatusCode::TOO_MANY_REQUESTS => {
                        let cache_value = cache.get_mut(self);
                        if let Some(mut cv) = cache_value {
                            cv.timestamp = Timestamp::now();
                            // Default to waiting 4 hrs if no Retry-After
                            let retry_after = r
                                .headers()
                                .get("retry-after")
                                .and_then(|v| v.to_str().ok())
                                .and_then(|s| s.parse::<i64>().ok())
                                .map(ToSpan::seconds)
                                .or(Some(4.hours()));
                            debug!(url=%self, response=?r, "got 429, using feed from cache");
                            cv.timestamp = Timestamp::now();
                            cv.retry_after = retry_after;
                            cv.body
                                .clone()
                                .ok_or(OpenringError::EmptyFeedError(self.as_str().to_string()))
                        } else {
                            Err(OpenringError::RateLimitError(self.as_str().to_string()))
                        }
                    }
                    unexpected => Err(OpenringError::UnexpectedStatusError {
                        url: self.as_str().to_string(),
                        status: unexpected.as_str().to_string(),
                    }),
                }
            }
            Err(e) => {
                warn!(url=%self.as_str(), error=%e, "failed to get feed.");
                Err(e.into())
            }
        };

        match body {
            Ok(feed_str) => match parser::parse(feed_str.as_bytes()) {
                Ok(feed) => Ok(feed),
                Err(e) => {
                    warn!(
                        url=%self.as_str(),
                        error=%e,
                        "failed to parse feed."
                    );
                    Err(e.into())
                }
            },
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use dashmap::DashMap;
    use jiff::{Timestamp, Unit};
    use proptest::prelude::*;
    use std::{sync::Arc, sync::OnceLock};
    use tokio::runtime::Runtime;
    use url::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::{cache::CacheValue, error::OpenringError};

    use super::FeedFetcher;

    // Import your crate items (adjust crate name if needed)
    // Cache alias from your crate: pub(crate) type Cache = DashMap<Url, CacheValue>;
    type Cache = DashMap<Url, CacheValue>;

    // create a global runtime you can reuse across test cases
    static RT: OnceLock<Runtime> = OnceLock::new();

    fn get_rt() -> &'static Runtime {
        RT.get_or_init(|| Runtime::new().expect("failed to create runtime"))
    }

    // Return a valid RSS 2.0 feed (with parameterized title)
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

    fn day_name_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("Mon".to_string()),
            Just("Tue".to_string()),
            Just("Wed".to_string()),
            Just("Thu".to_string()),
            Just("Fri".to_string()),
            Just("Sat".to_string()),
            Just("Sun".to_string()),
        ]
    }

    fn day_strategy() -> impl Strategy<Value = String> {
        any::<u8>().prop_map(|day| format!("{:02}", day % 31 + 1)) // Days between 01 and 31
    }

    fn month_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("Jan".to_string()),
            Just("Feb".to_string()),
            Just("Mar".to_string()),
            Just("Apr".to_string()),
            Just("May".to_string()),
            Just("Jun".to_string()),
            Just("Jul".to_string()),
            Just("Aug".to_string()),
            Just("Sep".to_string()),
            Just("Oct".to_string()),
            Just("Nov".to_string()),
            Just("Dec".to_string()),
        ]
    }

    fn year_strategy() -> impl Strategy<Value = String> {
        any::<u32>().prop_map(|year| format!("{}", year % 10000)) // Example: Limits to 4-digit years
    }

    fn hour_strategy() -> impl Strategy<Value = String> {
        any::<u8>().prop_map(|hour| format!("{:02}", hour % 24)) // Hours between 00 and 23
    }

    fn minute_strategy() -> impl Strategy<Value = String> {
        any::<u8>().prop_map(|minute| format!("{:02}", minute % 60)) // Minutes between 00 and 59
    }

    fn second_strategy() -> impl Strategy<Value = String> {
        any::<u8>().prop_map(|second| format!("{:02}", second % 60)) // Seconds between 00 and 59
    }

    fn last_modified_strategy() -> impl Strategy<Value = String> {
        (
            day_name_strategy(),
            day_strategy(),
            month_strategy(),
            year_strategy(),
            hour_strategy(),
            minute_strategy(),
            second_strategy(),
        )
            .prop_map(|(day_name, day, month, year, hour, minute, second)| {
                format!("{day_name}, {day} {month} {year} {hour}:{minute}:{second} GMT")
            })
    }

    proptest! {
        // 200 with arbitrary, non-empty body parses or returns parser error; doesn't crash.
        #[test]
        fn prop_success_parses_feed_without_crashing(feed_body in "\\PC*") {
            let res: Result<(), proptest::test_runner::TestCaseError> = get_rt().block_on(async {
                let server = MockServer::start().await;
                let body = if feed_body.trim().is_empty() {
                    "<?xml version=\"1.0\"?><rss><channel><title>x</title></channel></rss>".to_string()
                } else {
                    feed_body.clone()
                };

                Mock::given(method("GET"))
                    .and(path("/"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(body.clone()))
                    .mount(&server)
                    .await;

                let url = Url::parse(&server.uri()).unwrap();
                let cache = Arc::new(Cache::new());

                // Call the method under test
                let res = url.fetch_feed(&cache).await;

                // If parser fails we still accept Err(ParseError) as meaningful, but for typical feed bodies expect Ok
                if let Ok(feed) = res {
                    // sanity: parsed feed must be a Feed instance
                    let _ = feed;
                }
                Ok(())
            });
            res.unwrap();
        }

        // 304 Not Modified should use cached body
        #[test]
        fn prop_not_modified_uses_cache(cached_title in "[a-z]{1,50}") {
            let res: Result<(), proptest::test_runner::TestCaseError> = get_rt().block_on(async {
                let server = MockServer::start().await;

                Mock::given(method("GET")).and(path("/"))
                    .respond_with(ResponseTemplate::new(304))
                    .mount(&server)
                    .await;

                let url = Url::parse(&server.uri()).unwrap();
                let cache = Arc::new(Cache::new());

                let body = get_valid_rss_feed(&cached_title);
                let cv = CacheValue {
                    timestamp: Timestamp::now(),
                    retry_after: None,
                    last_modified: None,
                    etag: None,
                    body: Some(body.clone()),
                };
                cache.insert(url.clone(), cv);

                let feed = url.fetch_feed(&cache).await.expect("expected cached feed");
                prop_assert!(feed.title.as_ref().is_some_and(|t| t.content.contains(&cached_title)));
                Ok(())
            });
            res.unwrap();
        }

        // HTTP 429 uses cache and sets retry_after (if header present) or default 4 hours
        #[test]
        fn prop_too_many_requests_with_optional_retry(header_retry in prop::option::of(1u64..10_000u64)) {
            let res: Result<(), proptest::test_runner::TestCaseError> = get_rt().block_on(async {
                let server = MockServer::start().await;

                let mut template = ResponseTemplate::new(429);
                if let Some(r) = header_retry {
                    template = template.insert_header("retry-after", r.to_string());
                }

                Mock::given(method("GET")).and(path("/"))
                    .respond_with(template)
                    .mount(&server)
                    .await;

                let url = Url::parse(&server.uri()).unwrap();
                let cache = Arc::new(Cache::new());

                let feed_title = "cached429";

                let body = get_valid_rss_feed(feed_title);
                let cv = CacheValue {
                    timestamp: Timestamp::now(),
                    retry_after: None,
                    last_modified: None,
                    etag: None,
                    body: Some(body.clone()),
                };
                cache.insert(url.clone(), cv);

                let feed = url.fetch_feed(&cache).await.expect("expected cached feed on 429");
                prop_assert!(feed.title.as_ref().is_some_and(|t| t.content.contains(feed_title)));

                // Verify cache entry has retry_after set
                if let Some(entry) = cache.get(&url) {
                    prop_assert!(entry.retry_after.is_some());
                    // if header provided, it should match roughly the seconds; if not provided, default 4 hours expected
                    if header_retry.is_some() {
                        let span = entry.retry_after.unwrap();
                        prop_assert!(span.total(Unit::Second)? > 0.0);
                    } else {
                        let span = entry.retry_after.unwrap();
                        let span_secs = span.total(Unit::Second)?;
                        prop_assert!(span_secs >= 4.0 * 3600.0, "{:?} < 4 * 3600", span_secs);
                    }
                } else {
                    // If entry missing, fail
                    prop_assert!(false);
                }
                Ok(())
            });
            res.unwrap();
        }

        // Unexpected status results in UnexpectedStatusError
        #[test]
        fn prop_unexpected_status(code in 300u16..600u16) {
            prop_assume!(code != 200 && code != 304 && code != 429);

            let res: Result<(), proptest::test_runner::TestCaseError> = get_rt().block_on(async {
                let server = MockServer::start().await;

                Mock::given(method("GET"))
                    .and(path("/"))
                    .respond_with(ResponseTemplate::new(code))
                    .mount(&server)
                    .await;

                let url = Url::parse(&server.uri()).unwrap();
                let cache = Arc::new(Cache::new());

                let res = url.fetch_feed(&cache).await;
                prop_assert!(
                    matches!(
                        res, Err(OpenringError::UnexpectedStatusError{ .. })
                    ),
                    "Expected OpenringUnexpectedStatusError, got {:?} for status code {:?}",
                    res,
                    code
                );
                Ok(())
            });
            res.unwrap();
        }

        // Verifies If-None-Match and If-Modified-Since header is sent and properly quoted/normalized based on cache
        #[test]
        fn prop_sends_if_none_match_and_if_modified_since(
            etag_input in prop_oneof![
                // unquoted token
                "[A-Za-z0-9]{1,30}",
                // already quoted
                "\"[A-Za-z0-9]{1,30}\"",
                // weak etag
                "W/\"[A-Za-z0-9]{1,30}\""
            ],
            last_modified_input in last_modified_strategy(),
        ) {
            let res: Result<(), proptest::test_runner::TestCaseError> = get_rt().block_on(async {
                let server = MockServer::start().await;

                // capture the request to inspect headers
                let expected_etag = {
                    // normalize the input the same way production code would:
                    if (etag_input.starts_with('"') && etag_input.ends_with('"'))
                        || (etag_input.starts_with("W/\"") && etag_input.ends_with('"'))
                    {
                        etag_input.clone()
                    } else {
                        format!("\"{etag_input}\"")
                    }
                };

                // mock responds 304 so fetch_feed uses cache path that sends If-None-Match
                Mock::given(method("GET"))
                    .and(path("/"))
                    .respond_with(ResponseTemplate::new(304).append_header("etag", &etag_input))
                    .mount(&server)
                    .await;

                // create url and cache containing the etag
                let url = Url::parse(&server.uri()).unwrap();
                let cache = Arc::new(Cache::new());

                let body = get_valid_rss_feed("fake");
                let cv = CacheValue {
                    timestamp: Timestamp::now(),
                    retry_after: None,
                    last_modified: Some(last_modified_input.clone()),
                    etag: Some(expected_etag.clone()),
                    body: Some(body),
                };
                cache.insert(url.clone(), cv);

                // Make the request and inspect the mock server received requests
                let _ = url.fetch_feed(&cache).await?;

                // retrieve requests recorded by the mock server
                let received = server.received_requests().await.unwrap();
                // there should be at least one request
                prop_assert!(!received.is_empty());
                let first = &received[0];
                // header keys in recorded requests are lowercase
                let if_none_match = first
                    .headers
                    .get("if-none-match")
                    .ok_or_else(|| proptest::test_runner::TestCaseError::fail("missing If-None-Match header"))?;
                prop_assert_eq!(if_none_match.to_str()?, &expected_etag, "{} != {}", if_none_match.to_str()?, &expected_etag);
                let if_modified_since = first
                    .headers
                    .get("if-modified-since")
                    .ok_or_else(|| proptest::test_runner::TestCaseError::fail("missing If-Modified-Since header"))?;
                prop_assert_eq!(if_modified_since.to_str()?, &last_modified_input, "{} != {}", if_modified_since.to_str()?, &last_modified_input);
                Ok(())
            });
            res.unwrap();
        }
        // Verifies etag and last_modified metadata is saved to the cache on responses
        #[test]
        fn prop_sets_etag_and_last_modified_on_response(
            etag_input in prop_oneof![
                // unquoted token
                "[A-Za-z0-9]{1,30}",
                // already quoted
                "\"[A-Za-z0-9]{1,30}\"",
                // weak etag
                "W/\"[A-Za-z0-9]{1,30}\"",
            ],
            last_modified_input in last_modified_strategy(),
        ) {
            let res: Result<(), proptest::test_runner::TestCaseError> = get_rt().block_on(async {
                let server = MockServer::start().await;

                // capture the request to inspect headers
                let expected_etag = {
                    // normalize the input the same way production code would:
                    if (etag_input.starts_with('"') && etag_input.ends_with('"'))
                        || (etag_input.starts_with("W/\"") && etag_input.ends_with('"'))
                    {
                        etag_input.clone()
                    } else {
                        format!("\"{etag_input}\"")
                    }
                };

                let cache = Arc::new(Cache::new());

                let body = get_valid_rss_feed("fake");
                // mock responds 200 so fetch_feed updates cache
                Mock::given(method("GET"))
                    .and(path("/"))
                    .respond_with(ResponseTemplate::new(200)
                        .append_header("etag", &etag_input)
                        .append_header("last-modified", &last_modified_input)
                        .set_body_string(body)
                    )
                    .mount(&server)
                    .await;

                // create url and cache containing the etag
                let url = Url::parse(&server.uri()).unwrap();

                // Make the request and inspect the mock server received requests
                let _ = url.fetch_feed(&cache).await?;

                // retrieve requests recorded by the mock server
                let received = server.received_requests().await.unwrap();

                // there should be at least one request
                prop_assert!(!received.is_empty());

                // Verify cache entry has etag and last_modified set
                if let Some(entry) = cache.get(&url) {
                    prop_assert!(entry.etag.is_some());
                    if let Some(etag) = &entry.etag {
                        prop_assert_eq!(etag, &expected_etag);
                    }

                    prop_assert!(entry.last_modified.is_some());
                    if let Some(last_modified) = &entry.last_modified{
                        prop_assert_eq!(last_modified, &last_modified_input);
                    }
                } else {
                    // If entry missing, fail
                    prop_assert!(false);
                }

                Ok(())
            });
            res.unwrap();
        }
    }
}
