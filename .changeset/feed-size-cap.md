---
"openring": patch
---

**fix**: reject feeds larger than 64 MiB instead of buffering them.

A urls-file mistake pointing at a video or other huge file used to be downloaded in full into memory and then written into the cache file.
Responses declaring a Content-Length over 64 MiB are rejected up front, and chunked or compressed responses without a usable length are cut off as soon as the streamed body crosses the same limit.
Either way the run reports a clear "feed is too large" error for that feed; even full-content feeds typically run single-digit MiB.
