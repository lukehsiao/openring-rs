---
"openring": patch
---

**fix**: warn once per unusable feed instead of once per skipped entry.

A feed with many entries missing a link, title, or date printed one warning per entry on every run, flooding stderr now that warnings are visible by default.
The per-entry detail moved to debug level, and a single warning fires only when a feed contributes no articles at all, naming the feed and how many entries were unusable.
Entries excluded by your own `--before` filter never trigger the warning.
