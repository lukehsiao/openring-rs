---
"openring": patch
---

**fix**: article order is now deterministic when timestamps tie.

Feeds are fetched concurrently and finish in a different order every run, so articles sharing a publication timestamp could swap places between runs and churn otherwise-unchanged generated pages.
Ties are now broken by article link, so the same inputs always render in the same order.
