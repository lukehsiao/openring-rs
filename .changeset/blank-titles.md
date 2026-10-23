---
"openring": patch
---

**fix**: never render an article or source with an empty title.

A feed title made only of spaces, no-break spaces, or markup used to render as a blank source name, and an entry with such a title rendered as an article with no title. The source name now falls back to the feed's host, and such an entry is skipped like one with no title.
