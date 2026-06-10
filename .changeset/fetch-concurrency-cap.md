---
"openring": patch
---

**fix**: bound concurrent feed fetches so long URL lists don't drop feeds.

Every feed was fetched simultaneously, so a urls file with more entries than the process's file-descriptor limit (256 by default on macOS) made fetches fail with exhaustion errors and feeds silently vanish from the output.
At most 32 fetches are now in flight at a time, which keeps the network saturated while staying well below any sane descriptor limit.
