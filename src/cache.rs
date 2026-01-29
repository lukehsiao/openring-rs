use std::{
    cmp::Ordering,
    fs,
    io::{BufReader, BufWriter},
    path::Path,
    time::Duration,
};

use dashmap::DashMap;
use jiff::{Span, Timestamp, ToSpan};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use url::Url;

use crate::{args::Args, error::Result};

pub(crate) const OPENRING_CACHE_FILE: &str = ".openringcache";
const MAX_SPAN_SEC: i64 = 631_107_417_600;

/// Describes a feed fetch result that can be serialized to disk
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct CacheValue {
    pub(crate) timestamp: Timestamp,
    pub(crate) retry_after: Option<Span>,
    pub(crate) last_modified: Option<String>,
    pub(crate) etag: Option<String>,
    pub(crate) body: Option<String>,
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
        let f = fs::File::create(path)?;
        let w = BufWriter::new(f);
        serde_json::to_writer(w, self)?;
        Ok(())
    }

    fn load<T: AsRef<Path>>(path: T, max_age_secs: u64, now: Timestamp) -> Result<Cache> {
        let clamped_secs: i64 = max_age_secs.min(MAX_SPAN_SEC as u64).cast_signed();

        let f = fs::File::open(path)?;
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
/// This returns an `Option` as starting without a cache is a common scenario
/// and we silently discard errors on purpose.
pub(crate) fn load_cache(args: &Args) -> Option<Cache> {
    if !args.cache {
        return None;
    }

    // Discard entire cache if it hasn't been updated since `max_cache_age`.
    // This is an optimization, which avoids iterating over the file and
    // checking the age of each entry.
    match fs::metadata(OPENRING_CACHE_FILE) {
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

    let cache = Cache::load(
        OPENRING_CACHE_FILE,
        args.max_cache_age.as_secs(),
        Timestamp::now(),
    );
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
    use std::{fs::File, thread::sleep, time::Duration as StdDuration};

    use jiff::{Span, Timestamp};
    use proptest::prelude::*;
    use tempfile::NamedTempFile;
    use tempfile::TempDir;
    use url::Url;

    use super::*;

    fn bounded_timestamp(secs: i64) -> Timestamp {
        let secs = secs.clamp(Timestamp::MIN.as_second(), Timestamp::MAX.as_second());
        Timestamp::from_second(secs).expect("failed to clamp timestamp")
    }

    // A random but well‑formed URL (http or https) – enough for the cache key.
    fn url_gen() -> impl Strategy<Value = Url> {
        // Simple host + path generator; you can extend it if you need more variety.
        ("https?://", "[a-z]{1,10}\\.[a-z]{2,5}", "/[a-z]{0,15}")
            .prop_map(|(scheme, host, path)| format!("{scheme}{host}{path}"))
            .prop_filter_map("valid URL", |s| Url::parse(&s).ok())
    }

    // Random `CacheValue`.  All fields are optional except `timestamp`.
    fn cache_value_gen() -> impl Strategy<Value = CacheValue> {
        let ts = any::<i64>().prop_map(bounded_timestamp);

        // `None`  – no retry‑after header
        // `Some` – a non‑negative span built from a random i64
        let retry_after = prop_oneof![
            // None branch
            Just(None),
            // Some branch
            any::<i64>()
                .prop_map(|secs| Span::new().seconds(secs.clamp(0, MAX_SPAN_SEC)))
                .prop_map(Some)
        ];

        let opt_string = ".*".prop_map(|s| if s.is_empty() { None } else { Some(s) });

        (
            ts,
            retry_after,
            opt_string.clone(),
            opt_string.clone(),
            opt_string,
        )
            .prop_map(
                |(timestamp, retry_after, last_modified, etag, body)| CacheValue {
                    timestamp,
                    retry_after,
                    last_modified,
                    etag,
                    body,
                },
            )
    }

    proptest! {
        // spans_equal behaves as expected for equal and different spans
        #[test]
        fn spans_equal_behavior(a_secs in 0i64..1_000_000i64, b_secs in 0i64..1_000_000i64) {
            let a = Span::new().seconds(a_secs);
            let b = Span::new().seconds(b_secs);

            let eq = spans_equal(&a, &b);
            prop_assert_eq!(eq, a_secs == b_secs);
        }

        // Round-trip JSON when optional fields are all None
        #[test]
        fn round_trip_all_none_fields(url in url_gen()) {
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
            prop_assert_eq!(&*loaded_val, &cv);
        }

        #[test]
        fn load_clamps_max_age(url in url_gen(), value in cache_value_gen()) {
            // create file with single entry
            let cache: Cache = DashMap::new();
            cache.insert(url.clone(), value.clone());
            let tmp = NamedTempFile::new().expect("temp file");
            cache.store(tmp.path()).expect("store succeeds");

            // Use an enormous max_age (u128-sized) mapped to u64::MAX via API; ensure it doesn't overflow
            let loaded_large = Cache::load(tmp.path(), u64::MAX, Timestamp::now()).expect("load succeeds");
            let loaded_clamped = Cache::load(tmp.path(), MAX_SPAN_SEC as u64, Timestamp::now()).expect("load succeeds");

            // Both should contain the same entry (since clamp should cap max age)
            prop_assert!(loaded_large.contains_key(&url));
            prop_assert!(loaded_clamped.contains_key(&url));
        }

        #[test]
        fn round_trip_preserves_entries(
            // generate a vector of (Url, CacheValue) pairs
            entries in prop::collection::vec((url_gen(), cache_value_gen()), 0..100)
        ) {
            // Build a cache and insert the generated entries
            let cache: Cache = DashMap::new();
            for (url, value) in &entries {
                cache.insert(url.clone(), value.clone());
            }

            // Write to a temporary file
            let tmp = NamedTempFile::new().expect("temp file");
            cache.store(tmp.path()).expect("store succeeds");

            // Load with a very large max_age so nothing is filtered out
            let loaded = Cache::load(tmp.path(), u64::MAX, Timestamp::now()).expect("load succeeds");

            // The two maps must contain the same keys and values
            for (url, value) in entries {
                let loaded_val = loaded.get(&url).expect("key present after load");
                prop_assert_eq!(&*loaded_val, &value);
            }
        }

        #[test]
        fn age_filter_discards_old_entries(
            // generate a fresh timestamp (now) and a max_age in seconds, ensuring they can be subtracted
            now_secs in 10_000i64..Timestamp::MAX.as_second(),
            max_age in 0u32..10_000,
            // generate a mix of recent and old entries
            entries in prop::collection::vec(
                (url_gen(), cache_value_gen()),
                0..200
            )
        ) {
            // Freeze “now” for the test
            let now = bounded_timestamp(now_secs);
            let max_age_span = Span::new().seconds(i64::from(max_age));

            // Build a cache where half the entries are artificially old
            let cache: Cache = DashMap::new();

            for (i, (url, mut value)) in entries.into_iter().enumerate() {
                // Make every even index entry older than max_age
                if i % 2 == 0 {
                    value.timestamp = now - max_age_span - Span::new().seconds(1);
                } else {
                    value.timestamp = now;
                }
                cache.insert(url, value);
            }

            // Write to a temporary file
            let tmp = NamedTempFile::new().expect("temp file");
            cache.store(tmp.path()).expect("store succeeds");

            // Load with the generated max_age
            let loaded = Cache::load(tmp.path(), max_age.into(), now).expect("load succeeds");

            // Verify that only the “new” entries survived
            for entry in &cache {
                let url   = entry.key();    // &Url
                let value = entry.value(); // &CacheValue

                let cutoff = now - max_age_span;          // Timestamp that is `max_age` old
                let should_keep = value.timestamp > cutoff;
                let present = loaded.contains_key(url);
                prop_assert_eq!(present, should_keep);
            }
        }
    }
    #[test]
    fn load_cache_returns_none_when_cache_disabled() {
        let mut args = Args {
            cache: false,
            ..Default::default()
        };
        args.cache = false;
        assert!(super::load_cache(&args).is_none());
    }

    #[test]
    fn load_cache_returns_none_when_no_file() {
        let tmpdir = TempDir::new().expect("tempdir");
        let cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&tmpdir).expect("chdir");

        let args = Args {
            cache: true,
            ..Default::default()
        };
        // Ensure no cache file exists
        let _ = fs::remove_file(OPENRING_CACHE_FILE);
        assert!(super::load_cache(&args).is_none());

        std::env::set_current_dir(cwd).expect("restore cwd");
    }

    #[test]
    fn load_cache_discards_too_old_file_and_returns_none() {
        let tmpdir = TempDir::new().expect("tempdir");
        let cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&tmpdir).expect("chdir");

        // Create the cache file
        let file_path = tmpdir.path().join(OPENRING_CACHE_FILE);
        File::create(&file_path).expect("create cache file");

        // Ensure the file's mtime is at least in the past relative to the check
        sleep(StdDuration::from_millis(10));

        // Use a max_cache_age smaller than the sleep to make the file "too old"
        let args = Args {
            cache: true,
            max_cache_age: Duration::from_millis(1),
            ..Default::default()
        };

        // Should detect file too old and return None
        assert!(super::load_cache(&args).is_none());

        std::env::set_current_dir(cwd).expect("restore cwd");
    }

    #[test]
    fn load_cache_uses_recent_file_and_loads_entries() {
        let tmpdir = TempDir::new().expect("tempdir");
        let cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&tmpdir).expect("chdir");

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
        cache.store(OPENRING_CACHE_FILE).expect("store");

        let args = Args {
            cache: true,
            max_cache_age: Duration::from_hours(24),
            ..Default::default()
        };

        // Should return Some(Cache) and contain our entry
        let loaded = super::load_cache(&args).expect("some cache");
        assert!(loaded.contains_key(&url));
        let loaded_val = loaded.get(&url).expect("get");
        assert_eq!(&*loaded_val, &value);

        std::env::set_current_dir(cwd).expect("restore cwd");
    }
}
