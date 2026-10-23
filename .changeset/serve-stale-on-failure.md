---
"openring": patch
---

**feat**: serve the cached copy of a feed when fetching it fails.

A network error, any error status other than `404`, `410`, or `429`, or a response that is empty, too large, or not a feed no longer drops the feed from the output. The cached copy renders instead, with a yellow `Stale` line in a terminal or a warning in the log. A `404` or `410` still skips the feed with an error, because only you can fix a feed that is gone. A `429` uses the cached copy without a stale warning and opens a retry window. A body that is empty or not a feed also no longer replaces a good cached copy. Stale copies age out after `--max-cache-age`, counted from when the server last sent, confirmed, or rate-limited the feed.
