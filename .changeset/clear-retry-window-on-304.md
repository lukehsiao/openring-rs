---
"openring": patch
---

**fix**: a `304 Not Modified` now ends a stored `429` retry window, as a `2xx` already did.

Before, a feed that answered `304` after its retry window expired restarted that window on every run, so openring stopped asking the server and kept serving the cached feed.
