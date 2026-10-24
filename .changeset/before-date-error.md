---
"openring": patch
---

**fix**: say what failed when the `--before` date cannot start in the local time zone.

The error said "Failed to parse civil date.", but clap parses the date. The error only comes from finding the first instant of that date in the local time zone, so it now says that. `OpenringError` no longer converts from any `jiff::Error`, so the message cannot show up for some other date error.
