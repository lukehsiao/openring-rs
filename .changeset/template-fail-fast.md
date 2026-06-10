---
"openring": patch
---

**fix**: fail on a missing or invalid template before fetching any feeds.

A wrong `--template-file` path or a template syntax error used to surface only after every feed had been fetched, wasting the whole network round.
The template is now read and parsed up front, so those mistakes fail in milliseconds.
