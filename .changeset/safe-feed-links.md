---
"openring": patch
---

**fix(security)**: a feed can no longer inject script into the rendered page through a link or title.

A feed could set an article or homepage link to a `javascript:` or `data:` URL, and the bundled template writes links through `| safe`, so a crafted link could break out of the `href` attribute and run script on your site. Only `http` and `https` links are now used; any other link is skipped like a missing one, and a `'` in a link is percent-encoded, so links are safe in single-quoted attributes too. Titles now also escape quotes, so templates may put them inside quoted attributes. A template that renders `article.title` or `article.source_title` without `| safe` now shows an apostrophe as `&#39;` and a double quote as `&quot;`, as it already showed `&` as `&amp;`; add `| safe`, as the bundled template does.

Link choice is stricter as well. An empty link or one that points back at the feed itself is skipped. Comment, edit, enclosure, hub, and paging links are never used as an article or homepage link, and a broken first link falls back to the next one.
