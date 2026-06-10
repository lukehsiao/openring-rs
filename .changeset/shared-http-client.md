---
"openring": patch
---

**perf**: reuse one HTTP client across all feed fetches.

Every feed fetch used to build its own HTTP client, paying for TLS setup per feed and never reusing connections.
A single shared client now serves the whole run, so fetches share a connection pool.
