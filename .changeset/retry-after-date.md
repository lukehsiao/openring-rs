---
"openring": patch
---

**fix**: honor the HTTP-date form of `Retry-After`.

RFC 9110 allows `Retry-After` to carry either delta-seconds or an HTTP-date, but only the seconds form was parsed; a date fell back to the 4-hour default window.
Date values are now parsed and the retry window is computed relative to the fetch time, so well-behaved rate limiting servers are respected precisely.
