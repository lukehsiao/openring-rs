---
"openring": patch
---

**fix**: accept feeds containing empty Atom or iTunes text elements.

`feed-rs` 3.1.0 fails to parse an entire feed when any Atom or iTunes text element (such as `<summary type="html"><![CDATA[]]></summary>`, `<title></title>`, or `<itunes:summary><![CDATA[]]></itunes:summary>`) is empty or whitespace-only. `openring` now strips those empty elements and retries parsing when `feed-rs` reports missing text, restoring feeds such as Oxide and Friends and Andy Konwinski's blog while [feed-rs/feed-rs#325](https://github.com/feed-rs/feed-rs/issues/325) is pending upstream.
