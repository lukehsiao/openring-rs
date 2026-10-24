---
"openring": patch
---

**fix**: say why a feed fetch failed without repeating its URL.

A Stale or Error line, and the warning logged when progress bars are hidden, already name the feed, but the reason after it named the URL again, as in "The request feed at `…` received an unexpected error (HTTP 503)." The reason is now short, such as "server answered HTTP 503", "the body is empty", or "rate limited (HTTP 429) with nothing cached". Network errors no longer repeat the feed URL either, but when a redirect fails, such as a redirect loop, they name the URL where it stopped. A server that answers 304 for a feed with nothing cached now says so, instead of claiming the feed was empty.

`OpenringError` no longer has the `EmptyFeedError`, `FeedTooLargeError`, `RateLimitError`, `UnexpectedStatusError`, and `ParseFeedError` variants. They only ever described a single feed's fetch, which never ends a run.
