---
"openring": patch
---

**fix**: a failed cache write no longer aborts the run.

If the cache directory was unwritable (read-only home, full disk), openring errored out after fetching every feed but before rendering any output.
The cache is an optimization, so a failed write now logs a warning and the run continues, matching how unreadable caches are already handled at load time.
