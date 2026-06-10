---
"openring": patch
---

**fix**: `--per-source` now picks each feed's most recent articles, as documented.

The per-source cap used to take the first N entries in the order the feed listed them, but nothing in RSS or Atom guarantees newest-first, so feeds sorted oldest-first contributed their oldest posts.
Entries filtered out by `--before` (or missing required fields) also consumed the cap, which could leave a feed contributing nothing even when it had qualifying articles.
Each feed now contributes its N most recent qualifying entries by publication date.
