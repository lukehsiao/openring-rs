---
"openring": patch
---

**fix**: feeds in non-UTF-8 encodings no longer render as mojibake.

The HTTP response was decoded to UTF-8 text before parsing, but the feed parser honors the XML prolog, so a feed declaring `encoding="ISO-8859-1"` was decoded twice and its accented characters came out garbled.
The raw response bytes now go straight to the feed parser, which decodes them per the feed's own declaration.
The cache stores the body as base64-encoded bytes to support this; caches written by older versions are discarded once with a warning and rebuilt on the next run.
