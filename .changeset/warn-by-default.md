---
"openring": patch
---

**fix**: show warnings by default instead of hiding them behind `-v`.

Warnings about skipped entries, cache problems, and redirected feed URLs were only visible with `-v`, so the actionable ones went unseen.
Warnings now print by default; `-q` restores the old errors-only behavior and `-qq` silences everything.
Log lines also suspend the fetch progress bar while they print, so the two no longer splice together on a terminal.
