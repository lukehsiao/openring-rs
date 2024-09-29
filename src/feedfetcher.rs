use std::{sync::Arc, time::Duration};

use clap::{crate_name, crate_version};
use feed_rs::{model::Feed, parser};
use jiff::{Timestamp, ToSpan};
use tracing::{debug, warn};
use ureq::{Agent, AgentBuilder};
use url::Url;

use crate::{
    cache::{Cache, CacheValue},
    error::OpenringError,
};

pub(crate) trait FeedFetcher {
    /// Fetch a feed
    fn fetch_feed(&self, cache: &Arc<Cache>) -> Result<(Feed, Url), OpenringError>;
}

impl FeedFetcher for Url {
    /// Fetch a feed for a URL
    fn fetch_feed(&self, cache: &Arc<Cache>) -> Result<(Feed, Url), OpenringError> {
        let agent: Agent = AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!(crate_name!(), '/', crate_version!()))
            .build();
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
                    } else {
                        warn!(url = %self.as_str(), "empty feed");
                    }
                }
            }
        }

        let req = {
            let mut r = agent.get(self.as_str());
            // Add friendly headers if cache is available
            if let Some(ref cv) = cache_value {
                if let Some(last_modified) = &cv.last_modified {
                    r = r.set("If-Modified-Since", last_modified);
                }
                if let Some(etag) = &cv.etag {
                    r = r.set("If-None-Match", etag);
                }
            }
            debug!(url=%self, if_modified_since=r.header("if-modified-since"), etag=r.header("if-none-match"), "sending request");
            r
        };

        let body = match req.call() {
            Ok(r) => {
                // ETag values must have the actual quotes
                let etag = r.header("etag").map(|s| {
                    if (s.starts_with('"') && s.ends_with('"'))
                        || (s.starts_with("W/\"") && s.ends_with('"'))
                    {
                        s.to_string()
                    } else {
                        format!("\"{}\"", s)
                    }
                });
                let last_modified = r.header("last-modified").map(|s| s.to_string());
                let status = r.status();
                let mut body = r.into_string().ok();

                // Update cache
                if let Some(mut cv) = cache_value {
                    match status {
                        304 => {
                            debug!(url=%self, status=status, "got 304, using feed from cache");
                            body = cv.body.clone();
                        }
                        _ => {
                            debug!(url=%self, status=status, "cache hit, using feed from body");
                            cv.etag = etag;
                            cv.last_modified = last_modified;
                            cv.body = body.clone();
                        }
                    }
                    cv.timestamp = Timestamp::now();
                } else {
                    debug!(url=%self, status=status, "using feed from body and adding to cache");
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
            Err(e) => {
                if let Some(mut cv) = cache_value {
                    cv.timestamp = Timestamp::now();
                    match e {
                        ureq::Error::Status(status, r) if status == 429 => {
                            // Default to waiting 4 hrs if no Retry-After
                            let retry_after = r
                                .header("retry-after")
                                .map(|s| s.parse::<i64>().map(|n| n.seconds()).ok())
                                .unwrap_or(Some(4.hours()));
                            debug!(url=%self, status=status, retry_after=r.header("retry-after"), "got 429, using feed from cache");
                            dbg!(&retry_after);
                            cv.timestamp = Timestamp::now();
                            cv.retry_after = retry_after;
                            cv.body
                                .clone()
                                .ok_or(OpenringError::EmptyFeedError(self.as_str().to_string()))
                        }
                        _ => Err(Box::new(e).into()),
                    }
                } else {
                    warn!(url=%self.as_str(), error=%e, "failed to get feed.");
                    Err(Box::new(e).into())
                }
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
