---
"openring": patch
---

**fix**: recover from cache entries that have metadata but no body.

When a feed's body failed to download, the cache kept its `ETag`/`Last-Modified` anyway, so every later run sent a conditional request, got `304 Not Modified` back, and failed with "feed was empty" until the entry expired.
Conditional headers are now only sent when the cache actually has a body to serve, so the next run refetches the feed in full.
