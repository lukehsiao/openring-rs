---
"openring": patch
---

**fix**: resolve relative feed links per RFC 3986 instead of mangling them.

Links in feeds were resolved by gluing the href onto the feed's origin, so a path-relative href like `page.html` produced a mangled host such as `https://example.compage.html`, and a protocol-relative href like `//other.example/x` pointed at the wrong site.
Hrefs are now joined against the feed URL with standard URL resolution, so absolute, root-relative, path-relative, and protocol-relative links all land where the feed intended.
