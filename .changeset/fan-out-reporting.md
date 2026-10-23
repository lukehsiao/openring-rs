---
"openring": patch
---

**fix**: name the feed or page whose fetch task crashed.

A fetch or summary task that panicked used to be reported without saying which URL it was working on, and stayed listed as in progress. It now shows an `Error` line naming the URL, and logs a warning. Fetch and summary warnings outside a terminal now carry the `openring::fan_out` target instead of `openring`.
