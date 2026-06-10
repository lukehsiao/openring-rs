---
"openring": patch
---

**fix**: an empty feed response is no longer cached as servable content.

A 200 response with an empty body was stored in the cache as real content, so later runs sent conditional requests, received `304 Not Modified`, and kept serving the empty feed (as a confusing parse error) until the entry expired.
Empty bodies are now treated as no body at all: the run reports the feed as empty and the next run refetches it in full.
