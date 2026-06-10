---
"openring": patch
---

**feat**: warn when a feed URL redirects.

Redirects were followed silently forever, so a feed that moved kept working without the urls file ever being updated.
A fetch that lands on a different URL than requested now logs a warning naming the new location (visible with `-v`).
