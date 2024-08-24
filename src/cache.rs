use std::{cmp::Ordering, fs, path::Path, time::Duration};

use dashmap::DashMap;
use jiff::{Span, Timestamp, ToSpan};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use url::Url;

use crate::{args::Args, error::Result};

pub(crate) const OPENRING_CACHE_FILE: &str = ".openringcache";

/// Describes a feed fetch result that can be serialized to disk
#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct CacheValue {
    pub(crate) timestamp: Timestamp,
    pub(crate) retry_after: Option<Span>,
    pub(crate) last_modified: Option<String>,
    pub(crate) etag: Option<String>,
    pub(crate) body: Option<String>,
}

pub(crate) type Cache = DashMap<Url, CacheValue>;

pub(crate) trait StoreExt {
    /// Store the cache under the given path. Update access timestamps
    fn store<T: AsRef<Path>>(&self, path: T) -> Result<()>;

    /// Load cache from path. Discard entries older than `max_age_secs`
    fn load<T: AsRef<Path>>(path: T, max_age_secs: u64) -> Result<Cache>;
}

impl StoreExt for Cache {
    fn store<T: AsRef<Path>>(&self, path: T) -> Result<()> {
        let mut wtr = csv::WriterBuilder::new()
            .has_headers(false)
            .from_path(path)?;
        for result in self {
            wtr.serialize((result.key(), result.value()))?;
        }
        Ok(())
    }

    fn load<T: AsRef<Path>>(path: T, max_age_secs: u64) -> Result<Cache> {
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_path(path)?;

        let map = DashMap::new();
        let current_ts = Timestamp::now();
        for result in rdr.deserialize() {
            let (url, value): (Url, CacheValue) = result?;
            // Discard entries older than `max_age_secs`.
            // This allows gradually updating the cache over multiple runs.
            if (current_ts - value.timestamp).compare(i64::try_from(max_age_secs)?.seconds())?
                == Ordering::Less
            {
                map.insert(url, value);
            }
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

    let cache = Cache::load(OPENRING_CACHE_FILE, args.max_cache_age.as_secs());
    match cache {
        Ok(cache) => Some(cache),
        Err(e) => {
            warn!("Error while loading cache: {e}. Continuing without.");
            None
        }
    }
}
