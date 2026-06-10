---
"openring": patch
---

**fix**: don't crash on titleless feeds served from an IP address.

The source-title fallback assumed every feed URL has a domain name, so a feed fetched from an IP host (e.g. `http://127.0.0.1:8000/feed.xml`) that omitted its `<title>` crashed the whole run.
The fallback now uses the URL's host, whether that is a domain name or an IP address.
