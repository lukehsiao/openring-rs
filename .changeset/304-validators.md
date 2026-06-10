---
"openring": patch
---

**fix**: keep cached validators current when a `304 Not Modified` rotates them.

A 304 response may carry updated `ETag` or `Last-Modified` headers, but openring kept validating against the old ones, so a server that rotates validators would answer every later run with a full download.
Validators carried on a 304 now replace the cached ones; absent headers leave them untouched.
