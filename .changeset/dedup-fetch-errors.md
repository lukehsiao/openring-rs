---
"openring": patch
---

**fix**: report each fetch failure once, with its real cause.

Failed fetches logged a warning and then surfaced the same failure again as the per-feed error line, so every broken feed printed twice.
A body that fails mid-transfer also no longer masquerades as "the feed was empty": the underlying error (e.g. "error decoding response body") is what gets reported, and the previously cached copy of the feed is left intact instead of being overwritten by the failed attempt.
