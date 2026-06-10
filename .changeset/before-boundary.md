---
"openring": patch
---

**fix**: `--before` now excludes articles published exactly at the cutoff instant.

An article published at the very moment the `--before` date begins (midnight, system timezone) was included even though it is not strictly before that date.
The comparison is now inclusive of the boundary on the exclusion side, matching the documented "only include articles before this date".
