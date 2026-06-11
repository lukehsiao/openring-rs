---
"openring": patch
---

**feat**: per-feed weighting to keep prolific feeds from dominating

An optional integer after a feed URL (`https://example.com/feed.xml 7`) makes openring pick a random article from that feed's 7 newest instead of always its newest, so a daily blog can be made to compete like a weekly one.
If the random pick lands past the articles the feed actually has, the feed sits out that run, so sparse feeds participate proportionally.
A new `--seed` flag makes the random selection reproducible (e.g. `--seed "$(date +%Y%m%d)"` for builds that are stable within a day).

Implications for existing users: output is unchanged unless you add a weight.
A urls-file line containing a raw space previously parsed as one percent-encoded URL and now parses as URL-plus-weight (erroring unless the trailing token is a positive integer).
