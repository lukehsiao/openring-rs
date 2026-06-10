---
"openring": patch
---

**fix**: don't crash on cache files with extreme retry windows.

The 429 retry window check added the cached timestamp and retry span with arithmetic that panics on overflow, so a corrupt or hand-edited cache file could crash every subsequent run until the cache was deleted.
The check now uses checked arithmetic and falls back to the span's sign when the deadline is unrepresentable.
