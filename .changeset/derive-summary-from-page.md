---
"openring": patch
---

**Feature**: derive a summary from the article's page when the feed provides none.

Some feeds ship entries with no `<summary>` or `<content>`, which left the webring entry blank. `openring` now fetches such an article's own page and derives a summary from it: the page's `<meta name="description">` (or the `og:`/`twitter:` variants), falling back to the article's leading paragraphs. The text is whitespace-normalized, capped at 500 characters, and HTML-escaped so it renders safely through the template's `| safe` filter. Pages are fetched only for the articles that will actually render, after selection, and a fetch that fails or returns non-HTML leaves the summary empty rather than failing the run.
