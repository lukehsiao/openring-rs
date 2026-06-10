---
"openring": patch
---

**fix**: skip entries without links instead of aborting the whole run.

A single feed entry with no `<link>` used to abort the entire run with a confusing "bad title" error.
Such entries are now skipped with a warning, the same way entries missing a title or date already were.
