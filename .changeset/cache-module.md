---
"openring": patch
---

**refactor**: rework the cache around one module, with a new file format.

The cache now records, as absolute times, when each feed's server last sent, confirmed, or rate-limited it, and when a rate limit ends. The first run after upgrading cannot read the old file, logs `Error while loading cache`, and fetches every feed in full once. Later runs use the new file as before. The whole-file age check is gone; entries still expire individually after `--max-cache-age`. A `Retry-After` delay now counts from the start of the run rather than from the moment the 429 arrived, so on a run with many feeds a rate limit can end up to a few minutes early. `OpenringError::SerdeJsonError` is gone, since a cache that cannot be read or written only logs a warning.
