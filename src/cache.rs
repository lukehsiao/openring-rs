use std::{
    cmp::Ordering,
    fs,
    io::{BufReader, BufWriter},
    path::{Path, PathBuf},
    time::Duration,
};

use dashmap::DashMap;
use directories::ProjectDirs;
use jiff::{Span, Timestamp, ToSpan};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use url::Url;

use crate::{args::Args, error::Result};

pub(crate) const MAX_SPAN_SEC: i64 = 631_107_417_600;

/// Options for loading cache
#[derive(Copy, Clone, Debug)]
pub(crate) enum CachePath<'a> {
    Default,
    #[allow(dead_code)]
    Path(&'a Path),
}

/// Describes a feed fetch result that can be serialized to disk
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct CacheValue {
    pub(crate) timestamp: Timestamp,
    pub(crate) retry_after: Option<Span>,
    pub(crate) last_modified: Option<String>,
    pub(crate) etag: Option<String>,
    pub(crate) body: Option<String>,
}

/// Get the path to cache location.
pub(crate) fn get_cache_path() -> Option<PathBuf> {
    if let Some(proj_dirs) = ProjectDirs::from("dev", "hsiao", "openring") {
        return Some(proj_dirs.cache_dir().join("cache.json"));
    }
    None
}

fn spans_equal(a: &Span, b: &Span) -> bool {
    // The spans we generate contain only time units, so `compare` never
    // needs a relative datetime and cannot error.
    a.compare(b)
        .expect("time‑only spans never require a relative datetime")
        == Ordering::Equal
}

impl PartialEq for CacheValue {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp == other.timestamp
            && self.last_modified == other.last_modified
            && self.etag == other.etag
            && self.body == other.body
            && match (&self.retry_after, &other.retry_after) {
                (Some(a), Some(b)) => spans_equal(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}
impl Eq for CacheValue {}

pub(crate) type Cache = DashMap<Url, CacheValue>;

pub(crate) trait StoreExt {
    /// Store the cache under the given path. Update access timestamps
    fn store<T: AsRef<Path>>(&self, path: T) -> Result<()>;

    /// Load cache from path. Discard entries older than `max_age_secs`
    fn load<T: AsRef<Path>>(path: T, max_age_secs: u64, now: Timestamp) -> Result<Cache>;
}

impl StoreExt for Cache {
    fn store<T: AsRef<Path>>(&self, path: T) -> Result<()> {
        // Ensure the parent directory exists
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let f = fs::File::create(path)?;
        // Grab a lock to avoid multiple processes writing simultaneously
        f.lock()?;
        let w = BufWriter::new(f);
        serde_json::to_writer_pretty(w, self)?;
        Ok(())
    }

    fn load<T: AsRef<Path>>(path: T, max_age_secs: u64, now: Timestamp) -> Result<Cache> {
        let clamped_secs: i64 = max_age_secs.min(MAX_SPAN_SEC as u64).cast_signed();

        let f = fs::File::open(path)?;

        // Acquire a shared lock so multiple readers can coexist, but no writers
        f.lock_shared()?;

        let r = BufReader::new(f);

        let map: DashMap<Url, CacheValue> = serde_json::from_reader(r)?;

        // Remove entries older than max_age_secs
        let current_ts = now;
        let threshold = clamped_secs.seconds();
        let keys_to_remove: Vec<Url> = map
            .iter()
            .filter_map(|entry| {
                let v = entry.value();
                if (current_ts - v.timestamp).compare(threshold).ok()? == std::cmp::Ordering::Less {
                    None
                } else {
                    Some(entry.key().clone())
                }
            })
            .collect();

        for k in keys_to_remove {
            map.remove(&k);
        }

        Ok(map)
    }
}
/// Load cache (if exists and is still valid).
///
/// This returns an `Option` as starting without a cache is a common scenario
/// and we silently discard errors on purpose.
pub(crate) fn load_cache(args: &Args, cache_path: CachePath) -> Option<Cache> {
    if args.no_cache {
        return None;
    }
    let default_cache_path = get_cache_path();
    let cache_path = match cache_path {
        CachePath::Default if default_cache_path.is_none() => return None,
        CachePath::Default => default_cache_path.unwrap(),
        CachePath::Path(p) => p.to_path_buf(),
    };

    // Discard entire cache if it hasn't been updated since `max_cache_age`.
    // This is an optimization, which avoids iterating over the file and
    // checking the age of each entry.
    match fs::metadata(&cache_path) {
        Err(_e) => {
            // No cache found; silently start with empty cache
            return None;
        }
        Ok(metadata) => {
            let modified = metadata.modified().ok()?;
            let elapsed = modified.elapsed().ok()?;
            if elapsed > args.max_cache_age {
                warn!(
                    "Cache is too old (age: {:#?}, max age: {:#?}). Discarding and recreating.",
                    Duration::from_secs(elapsed.as_secs()),
                    Duration::from_secs(args.max_cache_age.as_secs())
                );
                return None;
            }
            info!(
                "Cache is recent (age: {:#?}, max age: {:#?}). Using.",
                Duration::from_secs(elapsed.as_secs()),
                Duration::from_secs(args.max_cache_age.as_secs())
            );
        }
    }

    let cache = Cache::load(cache_path, args.max_cache_age.as_secs(), Timestamp::now());
    match cache {
        Ok(cache) => Some(cache),
        Err(e) => {
            warn!("Error while loading cache: {e}. Continuing without.");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::{fs::File, thread::sleep, time::Duration as StdDuration};

    use jiff::{Span, Timestamp};
    use tempfile::NamedTempFile;
    use tempfile::TempDir;
    use url::Url;

    use hegel::TestCase;
    use hegel::extras::jiff as jiff_gs;
    use hegel::generators;

    use super::*;

    fn bounded_timestamp(secs: i64) -> Timestamp {
        let secs = secs.clamp(Timestamp::MIN.as_second(), Timestamp::MAX.as_second());
        Timestamp::from_second(secs).expect("failed to clamp timestamp")
    }

    // Cap on generated collection sizes. The cache is a flat map, so its
    // round-trip and age-filtering properties are fully exercised well below this;
    // a larger cap mostly inflates per-case entropy and runtime.
    const MAX_TEST_ENTRIES: usize = 50;

    // A well-formed URL, enough to act as a distinct cache key.
    #[hegel::composite]
    fn urls(tc: hegel::TestCase) -> Url {
        let s = tc.draw(generators::urls());
        Url::parse(&s).expect("generated string is a valid URL")
    }

    // A `CacheValue` with arbitrary fields. All are optional except `timestamp`,
    // and the retry span is kept within what jiff can represent.
    #[hegel::composite]
    fn cache_values(tc: hegel::TestCase) -> CacheValue {
        // retry_after only ever holds a time-only span in production, so we
        // generate seconds rather than jiff_gs::spans(): its calendar-unit spans
        // are out of domain and would trip the time-only `spans_equal`.
        let retry_after = tc.draw(generators::optional(
            generators::integers::<i64>()
                .min_value(0)
                .max_value(MAX_SPAN_SEC),
        ));
        // The string fields are opaque cache payloads; capping their length keeps
        // per-case entropy modest when many values populate a collection, while
        // still exercising escaping and unicode.
        let text = || generators::optional(generators::text().max_size(64));
        CacheValue {
            timestamp: tc.draw(jiff_gs::timestamps()),
            retry_after: retry_after.map(|secs| Span::new().seconds(secs)),
            last_modified: tc.draw(text()),
            etag: tc.draw(text()),
            body: tc.draw(text()),
        }
    }

    // `spans_equal` agrees with integer-seconds equality across the full range of
    // representable spans.
    #[hegel::test]
    fn spans_equal_behavior(tc: hegel::TestCase) {
        let a_secs = tc.draw(
            generators::integers::<i64>()
                .min_value(-MAX_SPAN_SEC)
                .max_value(MAX_SPAN_SEC),
        );
        let b_secs = tc.draw(
            generators::integers::<i64>()
                .min_value(-MAX_SPAN_SEC)
                .max_value(MAX_SPAN_SEC),
        );
        let a = Span::new().seconds(a_secs);
        let b = Span::new().seconds(b_secs);
        assert_eq!(spans_equal(&a, &b), a_secs == b_secs);
    }

    // A value with all-None optional fields survives a store/load round-trip.
    #[hegel::test]
    fn round_trip_all_none_fields(tc: hegel::TestCase) {
        let url = tc.draw(urls());
        let cache: Cache = DashMap::new();
        let cv = CacheValue {
            timestamp: Timestamp::now(),
            retry_after: None,
            last_modified: None,
            etag: None,
            body: None,
        };
        cache.insert(url.clone(), cv.clone());

        let tmp = NamedTempFile::new().expect("temp file");
        cache.store(tmp.path()).expect("store succeeds");

        let loaded = Cache::load(tmp.path(), u64::MAX, Timestamp::now()).expect("load succeeds");
        let loaded_val = loaded.get(&url).expect("key present after load");
        assert_eq!(&*loaded_val, &cv);
    }

    // An enormous max_age is clamped rather than overflowing, so the entry loads.
    #[hegel::test]
    fn load_clamps_max_age(tc: hegel::TestCase) {
        let url = tc.draw(urls());
        let value = tc.draw(cache_values());
        let cache: Cache = DashMap::new();
        cache.insert(url.clone(), value);
        let tmp = NamedTempFile::new().expect("temp file");
        cache.store(tmp.path()).expect("store succeeds");

        let loaded_large =
            Cache::load(tmp.path(), u64::MAX, Timestamp::now()).expect("load succeeds");
        let loaded_clamped =
            Cache::load(tmp.path(), MAX_SPAN_SEC as u64, Timestamp::now()).expect("load succeeds");

        assert!(loaded_large.contains_key(&url));
        assert!(loaded_clamped.contains_key(&url));
    }

    // Every entry round-trips identically through store/load. Iterating the cache
    // (rather than the raw inputs) sidesteps any duplicate-key collisions. Fewer
    // cases than the default since each one writes and reads a whole cache file.
    #[hegel::test(test_cases = 25)]
    fn round_trip_preserves_entries(tc: hegel::TestCase) {
        let n = tc.draw(
            generators::integers::<usize>()
                .min_value(0)
                .max_value(MAX_TEST_ENTRIES),
        );
        let cache: Cache = DashMap::new();
        for _ in 0..n {
            cache.insert(tc.draw(urls()), tc.draw(cache_values()));
        }

        let tmp = NamedTempFile::new().expect("temp file");
        cache.store(tmp.path()).expect("store succeeds");

        let loaded = Cache::load(tmp.path(), u64::MAX, Timestamp::now()).expect("load succeeds");
        for entry in &cache {
            let loaded_val = loaded.get(entry.key()).expect("key present after load");
            assert_eq!(&*loaded_val, entry.value());
        }
        assert_eq!(loaded.len(), cache.len());
    }

    // Loading discards exactly the entries older than max_age. Fewer cases than
    // the default since each one writes and reads a whole cache file.
    #[hegel::test(test_cases = 25)]
    fn age_filter_discards_old_entries(tc: hegel::TestCase) {
        let now_secs = tc.draw(
            generators::integers::<i64>()
                .min_value(10_000)
                .max_value(Timestamp::MAX.as_second()),
        );
        let max_age = tc.draw(generators::integers::<u32>().min_value(0).max_value(10_000));
        let n = tc.draw(
            generators::integers::<usize>()
                .min_value(0)
                .max_value(MAX_TEST_ENTRIES),
        );

        let now = bounded_timestamp(now_secs);
        let max_age_span = Span::new().seconds(i64::from(max_age));

        let cache: Cache = DashMap::new();
        for i in 0..n {
            let url = tc.draw(urls());
            let mut value = tc.draw(cache_values());
            // Make every even-indexed entry older than max_age.
            if i % 2 == 0 {
                value.timestamp = now - max_age_span - Span::new().seconds(1);
            } else {
                value.timestamp = now;
            }
            cache.insert(url, value);
        }

        let tmp = NamedTempFile::new().expect("temp file");
        cache.store(tmp.path()).expect("store succeeds");
        let loaded = Cache::load(tmp.path(), max_age.into(), now).expect("load succeeds");

        for entry in &cache {
            let cutoff = now - max_age_span;
            let should_keep = entry.value().timestamp > cutoff;
            assert_eq!(loaded.contains_key(entry.key()), should_keep);
        }
    }

    // Stateful model test: the cache (subject) must agree with a HashMap (model)
    // after every insert, get, and store/reload. This is the highest-value check
    // for a keyed data structure with persistence, exercising overwrite and
    // round-trip paths the isolated property tests above cannot.
    struct CacheModel {
        subject: Cache,
        model: HashMap<Url, CacheValue>,
        urls: Vec<Url>,
    }

    // Rule and invariant methods must take `TestCase` by value to match the
    // fn-pointer type hegel's `Rule::new` expects, but `tc.draw()` only borrows it.
    #[hegel::state_machine]
    #[expect(clippy::needless_pass_by_value)]
    impl CacheModel {
        #[rule]
        fn insert(&mut self, tc: TestCase) {
            let i = tc.draw(
                generators::integers::<usize>()
                    .min_value(0)
                    .max_value(self.urls.len() - 1),
            );
            let url = self.urls[i].clone();
            let value = tc.draw(cache_values());
            self.subject.insert(url.clone(), value.clone());
            self.model.insert(url, value);
        }

        #[rule]
        fn get(&mut self, tc: TestCase) {
            let i = tc.draw(
                generators::integers::<usize>()
                    .min_value(0)
                    .max_value(self.urls.len() - 1),
            );
            let url = &self.urls[i];
            let from_subject = self.subject.get(url).map(|e| e.value().clone());
            let from_model = self.model.get(url).cloned();
            assert_eq!(from_subject, from_model);
        }

        #[rule]
        fn store_reload(&mut self, _tc: TestCase) {
            let tmp = NamedTempFile::new().expect("temp file");
            self.subject.store(tmp.path()).expect("store succeeds");
            // u64::MAX keeps every entry, so a reload should be a pure round-trip.
            let reloaded =
                Cache::load(tmp.path(), u64::MAX, Timestamp::now()).expect("load succeeds");
            self.subject = reloaded;
        }

        #[invariant]
        fn agrees_with_model(&mut self, _tc: TestCase) {
            assert_eq!(self.subject.len(), self.model.len());
            for (url, value) in &self.model {
                let got = self.subject.get(url).expect("model key present in subject");
                assert_eq!(&*got, value);
            }
        }
    }

    #[hegel::test]
    fn cache_matches_hashmap_model(tc: hegel::TestCase) {
        let urls = vec![
            Url::parse("https://a.example/").unwrap(),
            Url::parse("https://b.example/").unwrap(),
            Url::parse("https://c.example/feed").unwrap(),
            Url::parse("https://d.example/atom").unwrap(),
        ];
        let machine = CacheModel {
            subject: DashMap::new(),
            model: HashMap::new(),
            urls,
        };
        hegel::stateful::run(machine, tc);
    }

    #[test]
    fn load_cache_returns_none_when_cache_disabled() {
        let tmp_cache_path = NamedTempFile::new().expect("temp file");
        let mut args = Args {
            no_cache: true,
            ..Default::default()
        };
        args.no_cache = true;
        assert!(super::load_cache(&args, CachePath::Path(tmp_cache_path.path())).is_none());
    }

    #[test]
    fn load_cache_returns_none_when_no_file() {
        let tmpdir = TempDir::new().expect("tempdir");
        let tmp_cache_path = tmpdir.path().join("nonexistent");
        let args = Args {
            no_cache: false,
            ..Default::default()
        };
        // Ensure no cache file exists
        let _ = fs::remove_file(&tmp_cache_path);
        assert!(super::load_cache(&args, CachePath::Path(tmp_cache_path.as_path())).is_none());
    }

    #[test]
    fn load_cache_discards_too_old_file_and_returns_none() {
        let tmp_cache_path = NamedTempFile::new().expect("temp file");
        File::create(&tmp_cache_path).expect("create cache file");

        // Ensure the file's mtime is at least in the past relative to the check
        sleep(StdDuration::from_millis(10));

        // Use a max_cache_age smaller than the sleep to make the file "too old"
        let args = Args {
            no_cache: false,
            max_cache_age: Duration::from_millis(1),
            ..Default::default()
        };

        // Should detect file too old and return None
        assert!(super::load_cache(&args, CachePath::Path(tmp_cache_path.path())).is_none());
    }

    #[test]
    fn load_cache_uses_recent_file_and_loads_entries() {
        let tmp_cache_path = NamedTempFile::new().expect("temp file");

        // Prepare a real cache JSON under the expected filename
        let url = Url::parse("https://example.test/").unwrap();
        let value = CacheValue {
            timestamp: Timestamp::now(),
            retry_after: None,
            last_modified: Some("Mon, 01 Jan 2000 00:00:00 GMT".into()),
            etag: Some("etag".into()),
            body: Some("body".into()),
        };
        let cache = Cache::new();
        cache.insert(url.clone(), value.clone());
        cache.store(&tmp_cache_path).expect("store");

        let args = Args {
            no_cache: false,
            max_cache_age: Duration::from_hours(24),
            ..Default::default()
        };

        // Should return Some(Cache) and contain our entry
        let loaded =
            super::load_cache(&args, CachePath::Path(tmp_cache_path.path())).expect("some cache");
        assert!(loaded.contains_key(&url));
        let loaded_val = loaded.get(&url).expect("get");
        assert_eq!(&*loaded_val, &value);
    }

    #[test]
    fn cache_round_trip_prunes_entries_that_are_old() {
        let tmp_cache_path = NamedTempFile::new().expect("temp file");

        // Prepare a real cache JSON under the expected filename
        let cache = Cache::new();
        let valid_url = Url::parse("https://example.test/").unwrap();
        let valid_value = CacheValue {
            timestamp: Timestamp::now(),
            retry_after: None,
            last_modified: Some("Mon, 01 Jan 2000 00:00:00 GMT".into()),
            etag: Some("etag".into()),
            body: Some("body".into()),
        };
        cache.insert(valid_url.clone(), valid_value.clone());

        // To old, should be filtered
        let expired_url = Url::parse("https://example2.test/").unwrap();
        let expired_value = CacheValue {
            timestamp: Timestamp::now() - Duration::from_hours(48),
            retry_after: None,
            last_modified: Some("Mon, 01 Jan 2000 00:00:00 GMT".into()),
            etag: Some("etag".into()),
            body: Some("body".into()),
        };
        cache.insert(expired_url.clone(), expired_value.clone());
        cache.store(&tmp_cache_path).expect("store");

        let args = Args {
            no_cache: false,
            max_cache_age: Duration::from_hours(24),
            ..Default::default()
        };

        // Should return Some(Cache) and contain only the valid entry
        let loaded =
            super::load_cache(&args, CachePath::Path(tmp_cache_path.path())).expect("some cache");
        assert!(loaded.contains_key(&valid_url));
        assert!(!loaded.contains_key(&expired_url));
        let loaded_val = loaded.get(&valid_url).expect("get");
        assert_eq!(&*loaded_val, &valid_value);

        // After loading with the config, storing again should prune old entries
        loaded.store(&tmp_cache_path).expect("store");
        let loaded =
            super::load_cache(&args, CachePath::Path(tmp_cache_path.path())).expect("some cache");
        assert!(loaded.contains_key(&valid_url));
        assert!(!loaded.contains_key(&expired_url));
    }
}
