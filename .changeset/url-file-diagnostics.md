---
"openring": patch
---

**fix**: handle unreadable URL files gracefully and point diagnostics at the right line.

A URL file that was not valid UTF-8 crashed openring instead of reporting a readable error.
When an invalid URL shared text with an earlier comment, the diagnostic underlined the comment instead of the bad line; spans are now computed from the line's actual position.
