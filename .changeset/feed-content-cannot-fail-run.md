---
"openring": patch
---

**fix**: nothing inside a fetched feed can fail the whole run anymore.

A feed whose homepage link could not be parsed, or an entry dated outside the range openring can represent, used to abort the run for every feed. A bad homepage link now falls back to the next candidate link and finally to the feed URL. An out-of-range entry is skipped like any other entry missing its link, title, or date. The warning that a feed contributed no articles now fires only when none of its entries is complete, not when `--before` filtered them all out. `OpenringError::UrlParseError` is gone, since nothing raises it.
