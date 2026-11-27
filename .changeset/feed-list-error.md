---
"openring": patch
---

**refactor**: replace `OpenringError`'s `FeedMissing`, `FeedUrlError`, and `FeedWeightError` variants with `OpenringError::FeedList`.

The `FeedUrlError` and `FeedWeightError` structs are no longer exported from `openring::error`. Feed list diagnostics now live in the private `feed_list` module behind `OpenringError::FeedList`, keeping the rendered messages, spans, help text, and `openring::*` diagnostic codes unchanged.
