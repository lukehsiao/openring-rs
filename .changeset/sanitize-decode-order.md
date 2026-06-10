---
"openring": patch
---

**fix**: stop entity-encoded markup from reaching templates live, and sanitize titles.

Summaries were sanitized and then entity-decoded, so harmless text like `&lt;script&gt;` in a feed turned back into a live `<script>` tag, which the default template embeds raw via `| safe`.
Entities are now decoded before sanitizing, so the output is guaranteed safe HTML.
Article titles and source titles, which the default template also marks `| safe`, were not sanitized at all; they are now reduced to plain text with all markup stripped.
