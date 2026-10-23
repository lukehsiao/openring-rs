---
"openring": patch
---

**fix**: respect `NO_COLOR` in the progress bars and status lines.

Log lines already turned color off when `NO_COLOR` was set, but the bar labels and the `Fetched`, `Stale`, and `Error` words stayed colored. They now follow the same rule: any non-empty `NO_COLOR` turns color off.
