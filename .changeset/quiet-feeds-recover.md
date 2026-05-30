---
"openring": patch
---

**fix**: don't crash on feeds that omit a `<title>`.

A feed whose top-level element has no `<title>` previously panicked `openring`,
aborting the entire run even when every other feed was fine. Such feeds are now
handled like any other: the source title falls back to the feed's domain, and
the source link falls back to the feed's links (or the feed URL itself).
