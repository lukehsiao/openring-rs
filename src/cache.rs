//! The feed cache module: what openring remembers about each Feed between
//! runs, and every rule about that memory. It decides when a request is
//! skipped because a Retry window is open, which Validators a request
//! carries, how each answer changes what is stored, when an entry is too old
//! to keep, and how the whole cache lives on disk.
//!
//! Callers ask two questions. Before a fetch, [`Cache::plan`] says whether
//! to serve the cached Feed or send a request, and with which Validators.
//! After the server answers, [`Cache::record`] takes the [`Outcome`] and
//! returns the body to parse. Neither question touches HTTP, so tests replay
//! whole sequences of outcomes against a stopped clock.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use dashmap::DashMap;
use directories::ProjectDirs;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use url::Url;

use crate::{error::OpenringError, fs::Fs};

/// A Feed's raw document, shared rather than copied between the cache and
/// the fetch that serves it. It stays bytes, not text: the feed parser must
/// see the original encoding, since a latin-1 feed pre-decoded to UTF-8
/// turns to mojibake when the parser honors the XML prolog.
pub(crate) type Body = Arc<Vec<u8>>;

/// The `ETag` and `Last-Modified` values a server sent with a Feed.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Validators {
    pub(crate) etag: Option<String>,
    pub(crate) last_modified: Option<String>,
}

/// What to do about a Feed before fetching it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// A Retry window is open: serve this cached body and send nothing.
    Serve(Body),
    /// Send a request carrying these Validators. They are empty when the
    /// cache holds nothing for the Feed.
    Request(Validators),
}

/// What the server said about a Feed.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// A new body that parsed as a feed, so it is not empty, with the
    /// Validators that came with it.
    Fresh {
        body: Vec<u8>,
        validators: Validators,
    },
    /// The body has not changed. The server may still send new Validators.
    NotModified(Validators),
    /// Too many requests: ask again no sooner than `until`.
    RateLimited { until: Timestamp },
    /// The fetch failed for a reason that may pass: the network, the
    /// server, or a body that was too large, empty, or not a feed. Serve the
    /// cached body if there is one, and change nothing, so the entry still
    /// ages out from when the server last answered.
    Failed,
}

/// What the cache holds for one Feed.
#[derive(Serialize, Deserialize, Debug)]
struct Entry {
    /// When the server last answered. Entries older than the maximum cache
    /// age are dropped on load.
    fetched_at: Timestamp,
    /// The end of the Retry window, if one is open or was.
    retry_until: Option<Timestamp>,
    #[serde(flatten)]
    validators: Validators,
    #[serde(with = "body_base64")]
    body: Body,
}

/// Serialize the body as base64: `serde_json` would otherwise render a byte
/// vector as an integer array, one element per byte.
mod body_base64 {
    use std::sync::Arc;

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        body: &super::Body,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(body.as_slice()))
    }

    /// An empty body fails to decode, as no stored feed is empty, so a
    /// hand-edited file cannot make an empty feed servable.
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<super::Body, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let decoded = STANDARD.decode(encoded).map_err(serde::de::Error::custom)?;
        if decoded.is_empty() {
            return Err(serde::de::Error::custom("empty body"));
        }
        Ok(Arc::new(decoded))
    }
}

/// Every Feed the cache remembers. Clones share the same entries, so
/// concurrent fetches can each hold one.
#[derive(Debug, Clone, Default)]
pub(crate) struct Cache {
    entries: Arc<DashMap<Url, Entry>>,
}

impl Cache {
    /// Where the cache lives by default, if the platform has a cache
    /// directory.
    pub(crate) fn default_path() -> Option<PathBuf> {
        ProjectDirs::from("dev", "hsiao", "openring")
            .map(|dirs| dirs.cache_dir().join("cache.json"))
    }

    /// Load the cache saved at `path`, dropping entries whose server last
    /// answered `max_age` or longer before `now`.
    ///
    /// The cache is purely an optimization, so this never fails. A missing
    /// file starts an empty cache quietly. An unreadable or undecodable file
    /// starts an empty cache with a warning. That includes caches written by
    /// older versions in another format, which is the intended migration.
    pub(crate) fn load(fs: &Fs, path: &Path, max_age: Duration, now: Timestamp) -> Self {
        let loaded = fs.read_locked(path).and_then(|text| {
            serde_json::from_str::<DashMap<Url, Entry>>(&text).map_err(io::Error::other)
        });
        let entries = match loaded {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "no cache yet; starting empty");
                return Self::default();
            }
            Err(e) => {
                warn!("Error while loading cache: {e}. Continuing without.");
                return Self::default();
            }
        };
        // A maximum age too large for jiff to subtract keeps every entry.
        if let Some(cutoff) = SignedDuration::try_from(max_age)
            .ok()
            .and_then(|age| now.checked_sub(age).ok())
        {
            entries.retain(|_, entry| entry.fetched_at > cutoff);
        }
        Self {
            entries: Arc::new(entries),
        }
    }

    /// Save the cache to `path`, best-effort.
    ///
    /// A failed write (read-only cache directory, full disk) only logs a
    /// warning: failing the run here would throw away a whole round of
    /// fetches right before the output is rendered.
    pub(crate) fn save(&self, fs: &Fs, path: &Path) {
        let result = fs.write_locked(path, |w| {
            serde_json::to_writer_pretty(w, &*self.entries).map_err(io::Error::other)
        });
        if let Err(e) = result {
            warn!("Error while storing cache: {e}. Continuing without.");
        }
    }

    /// What to do about the Feed at `url` before fetching it at `now`.
    pub(crate) fn plan(&self, url: &Url, now: Timestamp) -> Plan {
        let Some(entry) = self.entries.get(url) else {
            return Plan::Request(Validators::default());
        };
        if entry.retry_until.is_some_and(|until| until > now) {
            debug!(url = url.as_str(), retry_until = ?entry.retry_until, "retry window open; serving the cached feed");
            return Plan::Serve(Arc::clone(&entry.body));
        }
        Plan::Request(entry.validators.clone())
    }

    /// Record what the server said about the Feed at `url` at `now`, and
    /// return the body to parse.
    ///
    /// # Errors
    ///
    /// Returns [`OpenringError::EmptyFeedError`] when the cache holds no
    /// body to serve, and [`OpenringError::RateLimitError`] when the server
    /// is rate limiting a Feed the cache knows nothing about.
    pub(crate) fn record(
        &self,
        url: &Url,
        outcome: Outcome,
        now: Timestamp,
    ) -> Result<Body, OpenringError> {
        let empty = || OpenringError::EmptyFeedError(url.as_str().to_string());
        match outcome {
            Outcome::Fresh { body, validators } => {
                let body = Arc::new(body);
                self.entries.insert(
                    url.clone(),
                    Entry {
                        fetched_at: now,
                        // A normal answer ends any Retry window.
                        retry_until: None,
                        validators,
                        body: Arc::clone(&body),
                    },
                );
                Ok(body)
            }
            Outcome::NotModified(validators) => {
                let mut entry = self.entries.get_mut(url).ok_or_else(empty)?;
                entry.fetched_at = now;
                // "Not modified" is a normal answer too, so it ends any Retry
                // window just as a fresh body does.
                entry.retry_until = None;
                // RFC 7232 lets the answer rotate the Validators it confirms.
                // Absent ones leave the stored ones in place.
                if validators.etag.is_some() {
                    entry.validators.etag = validators.etag;
                }
                if validators.last_modified.is_some() {
                    entry.validators.last_modified = validators.last_modified;
                }
                Ok(Arc::clone(&entry.body))
            }
            Outcome::RateLimited { until } => {
                let mut entry = self
                    .entries
                    .get_mut(url)
                    .ok_or_else(|| OpenringError::RateLimitError(url.as_str().to_string()))?;
                entry.fetched_at = now;
                entry.retry_until = Some(until);
                Ok(Arc::clone(&entry.body))
            }
            Outcome::Failed => self
                .entries
                .get(url)
                .map(|entry| Arc::clone(&entry.body))
                .ok_or_else(empty),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

    use hegel::{
        TestCase,
        generators::{self, Generator},
    };
    use jiff::{SignedDuration, Timestamp};
    use url::Url;

    use super::{Body, Cache, Outcome, Plan, Validators};
    use crate::{error::OpenringError, fs::Fs, test_support::warnings_logged};

    const CACHE_PATH: &str = "/cache/cache.json";

    fn url() -> Url {
        Url::parse("https://feed.example/feed.xml").unwrap()
    }

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn fresh(etag: Option<&str>) -> Outcome {
        Outcome::Fresh {
            body: b"feed".to_vec(),
            validators: Validators {
                etag: etag.map(str::to_string),
                last_modified: None,
            },
        }
    }

    #[test]
    fn a_missing_cache_file_starts_empty_without_a_warning() {
        let fs = Fs::create_null([]);
        let logs = warnings_logged(|| {
            let cache = Cache::load(
                &fs,
                Path::new(CACHE_PATH),
                Duration::MAX,
                at("2026-01-01T00:00:00Z"),
            );
            assert_eq!(
                cache.plan(&url(), at("2026-01-01T00:00:00Z")),
                Plan::Request(Validators::default())
            );
        });
        assert_eq!(logs, "");
    }

    // No stored feed is empty, so an empty body, as only a hand-edited file
    // holds, fails to load like any other malformed file: the cache starts
    // empty with a warning, and nothing is served from it.
    #[test]
    fn an_empty_stored_body_discards_the_file() {
        let file = r#"{"https://feed.example/feed.xml":{"fetched_at":"2026-01-01T00:00:00Z","retry_until":"2027-01-01T00:00:00Z","last_modified":null,"etag":"\"v1\"","body":""}}"#;
        let fs = Fs::create_null([(CACHE_PATH, file)]);
        let now = at("2026-01-01T00:00:01Z");
        let logs = warnings_logged(|| {
            let cache = Cache::load(&fs, Path::new(CACHE_PATH), Duration::MAX, now);
            assert_eq!(
                cache.plan(&url(), now),
                Plan::Request(Validators::default())
            );
        });
        assert!(logs.contains("Error while loading cache"), "{logs}");
    }

    // A cache written by an older version uses other field names. It fails
    // to decode, so the run starts with an empty cache and refetches every
    // Feed once.
    #[test]
    fn a_cache_in_an_older_format_is_discarded_with_a_warning() {
        let old = r#"{"https://feed.example/feed.xml":{"timestamp":"2026-01-01T00:00:00Z","retry_after":null,"last_modified":null,"etag":"\"v1\"","body":"PHJzcy8+"}}"#;
        let fs = Fs::create_null([(CACHE_PATH, old)]);
        let logs = warnings_logged(|| {
            let cache = Cache::load(
                &fs,
                Path::new(CACHE_PATH),
                Duration::MAX,
                at("2026-01-01T00:00:00Z"),
            );
            assert_eq!(
                cache.plan(&url(), at("2026-01-01T00:00:00Z")),
                Plan::Request(Validators::default())
            );
        });
        assert!(logs.contains("Error while loading cache"), "{logs}");
    }

    #[test]
    fn a_failed_save_warns_instead_of_failing() {
        // The parent "directory" is a file, so the write cannot succeed.
        let blocker = tempfile::NamedTempFile::new().unwrap();
        let path = blocker.path().join("cache.json");
        let cache = Cache::default();
        cache
            .record(&url(), fresh(None), at("2026-01-01T00:00:00Z"))
            .unwrap();

        let logs = warnings_logged(|| cache.save(&Fs::create(), &path));
        assert!(logs.contains("Error while storing cache"), "{logs}");
    }

    #[test]
    fn a_saved_cache_round_trips_through_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let now = at("2026-01-01T00:00:00Z");
        let cache = Cache::default();
        cache.record(&url(), fresh(Some("\"v1\"")), now).unwrap();

        cache.save(&Fs::create(), &path);
        let loaded = Cache::load(&Fs::create(), &path, Duration::MAX, now);
        assert_eq!(
            loaded
                .record(&url(), Outcome::NotModified(Validators::default()), now)
                .unwrap(),
            Arc::new(b"feed".to_vec())
        );
        // Prove the file holds JSON a person can read, with the body in
        // base64 rather than one number per byte.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"fetched_at\""), "{text}");
        assert!(text.contains("\"ZmVlZA==\""), "{text}");
    }

    // A model of the cache in plain terms, one entry per Feed, that the real
    // cache must agree with after any sequence of answers, clock moves, and
    // save-then-load round trips.
    #[derive(Debug)]
    struct ModelEntry {
        fetched_at: Timestamp,
        retry_until: Option<Timestamp>,
        validators: Validators,
        body: Vec<u8>,
    }

    #[derive(Debug, PartialEq)]
    enum Served {
        Body(Vec<u8>),
        Empty,
        RateLimited,
    }

    fn served(result: Result<Body, OpenringError>) -> Served {
        match result {
            Ok(body) => Served::Body(body.to_vec()),
            Err(OpenringError::EmptyFeedError(_)) => Served::Empty,
            Err(OpenringError::RateLimitError(_)) => Served::RateLimited,
            Err(e) => panic!("unexpected error {e:?}"),
        }
    }

    struct CacheMachine {
        subject: Cache,
        model: HashMap<Url, ModelEntry>,
        urls: Vec<Url>,
        now: Timestamp,
        fs: Fs,
    }

    impl CacheMachine {
        fn pick_url(&self, tc: &TestCase) -> Url {
            let i = tc.draw(generators::integers::<usize>().max_value(self.urls.len() - 1));
            self.urls[i].clone()
        }

        fn draw_validators(tc: &TestCase) -> Validators {
            Validators {
                etag: tc.draw(generators::optional(generators::text().max_size(16))),
                last_modified: tc.draw(generators::optional(generators::text().max_size(16))),
            }
        }
    }

    // Rule methods take `TestCase` by value to match the fn-pointer type
    // hegel's `Rule::new` expects, but `tc.draw()` only borrows it.
    #[hegel::state_machine]
    #[expect(clippy::needless_pass_by_value)]
    impl CacheMachine {
        #[rule]
        fn advance_the_clock(&mut self, tc: TestCase) {
            // Up to three days, so Retry windows and the maximum age both
            // open and close during a run of rules.
            let secs = tc.draw(
                generators::integers::<i64>()
                    .min_value(0)
                    .max_value(3 * 86_400),
            );
            self.now += SignedDuration::from_secs(secs);
        }

        #[rule]
        fn record_fresh(&mut self, tc: TestCase) {
            let url = self.pick_url(&tc);
            // A fresh body parsed as a feed, so it is never empty.
            let body = tc.draw(
                generators::vecs(generators::integers::<u8>())
                    .min_size(1)
                    .max_size(16),
            );
            let validators = Self::draw_validators(&tc);
            let got = served(self.subject.record(
                &url,
                Outcome::Fresh {
                    body: body.clone(),
                    validators: validators.clone(),
                },
                self.now,
            ));

            self.model.insert(
                url,
                ModelEntry {
                    fetched_at: self.now,
                    retry_until: None,
                    validators,
                    body: body.clone(),
                },
            );
            assert_eq!(got, Served::Body(body));
        }

        #[rule]
        fn record_not_modified(&mut self, tc: TestCase) {
            let url = self.pick_url(&tc);
            let validators = Self::draw_validators(&tc);
            let got = served(self.subject.record(
                &url,
                Outcome::NotModified(validators.clone()),
                self.now,
            ));

            let expected = match self.model.get_mut(&url) {
                Some(entry) => {
                    entry.fetched_at = self.now;
                    entry.retry_until = None;
                    if validators.etag.is_some() {
                        entry.validators.etag = validators.etag;
                    }
                    if validators.last_modified.is_some() {
                        entry.validators.last_modified = validators.last_modified;
                    }
                    Served::Body(entry.body.clone())
                }
                None => Served::Empty,
            };
            assert_eq!(got, expected);
        }

        #[rule]
        fn record_rate_limited(&mut self, tc: TestCase) {
            let url = self.pick_url(&tc);
            // A window that may already be over, as a Retry-After date in
            // the past gives.
            let secs = tc.draw(
                generators::integers::<i64>()
                    .min_value(-3_600)
                    .max_value(36_000),
            );
            let until = self.now + SignedDuration::from_secs(secs);
            let got = served(
                self.subject
                    .record(&url, Outcome::RateLimited { until }, self.now),
            );

            let expected = match self.model.get_mut(&url) {
                Some(entry) => {
                    entry.fetched_at = self.now;
                    entry.retry_until = Some(until);
                    Served::Body(entry.body.clone())
                }
                None => Served::RateLimited,
            };
            assert_eq!(got, expected);
        }

        #[rule]
        fn record_failed(&mut self, tc: TestCase) {
            let url = self.pick_url(&tc);
            let got = served(self.subject.record(&url, Outcome::Failed, self.now));

            // A failure serves what is cached and changes nothing.
            let expected = self
                .model
                .get(&url)
                .map_or(Served::Empty, |entry| Served::Body(entry.body.clone()));
            assert_eq!(got, expected);
        }

        #[rule]
        fn plan(&mut self, tc: TestCase) {
            let url = self.pick_url(&tc);
            let expected = match self.model.get(&url) {
                Some(ModelEntry {
                    body,
                    retry_until: Some(until),
                    ..
                }) if *until > self.now => Plan::Serve(Arc::new(body.clone())),
                Some(entry) => Plan::Request(entry.validators.clone()),
                None => Plan::Request(Validators::default()),
            };
            assert_eq!(self.subject.plan(&url, self.now), expected);
        }

        #[rule]
        fn save_and_load(&mut self, tc: TestCase) {
            let max_age = tc.draw(hegel::one_of!(
                generators::integers::<u64>()
                    .min_value(1)
                    .max_value(4 * 86_400)
                    .map(Duration::from_secs),
                generators::just(Duration::MAX)
            ));
            let path = Path::new(CACHE_PATH);
            self.subject.save(&self.fs, path);
            self.subject = Cache::load(&self.fs, path, max_age, self.now);

            // The model keeps an entry only while its server answered less
            // than `max_age` ago.
            if let Ok(age) = SignedDuration::try_from(max_age) {
                let now = self.now;
                self.model
                    .retain(|_, entry| entry.fetched_at.duration_until(now) < age);
            }
        }
    }

    #[hegel::test]
    fn the_cache_agrees_with_its_model(tc: TestCase) {
        let machine = CacheMachine {
            subject: Cache::default(),
            model: HashMap::new(),
            urls: vec![
                Url::parse("https://a.example/feed.xml").unwrap(),
                Url::parse("https://b.example/atom").unwrap(),
                Url::parse("https://c.example/").unwrap(),
            ],
            // Whole seconds between 2000 and 2100, so a day of clock moves
            // never leaves the range jiff can represent.
            now: Timestamp::from_second(
                tc.draw(
                    generators::integers::<i64>()
                        .min_value(946_684_800)
                        .max_value(4_102_444_800),
                ),
            )
            .unwrap(),
            fs: Fs::create_null([]),
        };
        hegel::stateful::machine(machine).run(tc);
    }
}
