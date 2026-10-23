---
"openring": patch
---

**fix**: point at the offending weight when a feed is listed with two weights across `-s` and the urls file.

A conflict between two `-s` values, or between `-s` and the urls file, used to print only the URL and both weights. It now shows the same labeled diagnostic as a conflict inside the urls file, pointing at the second weight and naming the first. `OpenringError::ConflictingWeightError` is gone from the library.
