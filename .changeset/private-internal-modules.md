---
"openring": patch
---

**refactor**: make the `cache`, `feedfetcher`, and `summarize` modules and the `Article` type private.

They were public by accident and never formed a library interface: `Article` had no public fields and no public function took or returned it. The library now exposes only `run`, `args`, `error`, and `progress`, which is what the `openring` binary uses.
