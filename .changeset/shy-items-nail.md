---
"openring": patch
---

**fix**: harden feed fetching against edge-case, rate-limited feeds.

- A feed that responds with HTTP 429 and an extremely large `Retry-After` value
  no longer crashes `openring`; the retry window is now capped.
- A feed that was rate-limited (429) now re-fetches promptly once it recovers,
  instead of continuing to serve a stale cached copy until the old retry window
  elapsed.
