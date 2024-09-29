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
    async fn fetch_feed(&self, cache: &Arc<Cache>) -> Result<(Feed, Url), OpenringError>;
}

impl FeedFetcher for Url {
    /// Fetch a feed for a URL
    async fn fetch_feed(&self, cache: &Arc<Cache>) -> Result<(Feed, Url), OpenringError> {
        let client: Client = ClientBuilder::new()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!(crate_name!(), '/', crate_version!()))
            .build()?;
        let cache_value = cache.get_mut(self);

        // Respect Retry-After Header if set in cache
        if let Some(ref cv) = cache_value {
            if let Some(retry) = cv.retry_after {
                if cv.timestamp + retry > Timestamp::now() {
                    debug!(timestamp=%cv.timestamp, retry_after=%retry, "skipping request due to 429, using feed from cache");

                    // TODO: This is just copy-pasted, should be reused
                    if let Some(ref feed_str) = cv.body {
                        return match parser::parse(feed_str.as_bytes()) {
                            Ok(feed) => Ok((feed, self.clone())),
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
            }
        }

        let req = {
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
                        body.ok_or(OpenringError::EmptyFeedError(self.as_str().to_string()))
                    }
                    StatusCode::TOO_MANY_REQUESTS => {
                        if let Some(mut cv) = cache_value {
                            cv.timestamp = Timestamp::now();
                            // Default to waiting 4 hrs if no Retry-After
                            let retry_after = r
                                .headers()
                                .get("retry-after")
                                .and_then(|retry_value| {
                                    retry_value.to_str().ok().map(|retry_str| {
                                        retry_str.parse::<i64>().map(ToSpan::seconds).ok()
                                    })
                                })
                                .unwrap_or(Some(4.hours()));
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
                Ok(feed) => Ok((feed, self.clone())),
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
