---
"openring": patch
---

**fix**: keep a seeded build stable when another weighted feed is down, and bound the cost of a huge weight.

Every weighted feed drew from one shared random stream, in URL order, so one weighted feed failing to fetch changed the picks of every weighted feed after it, breaking `--seed "$(date +%Y%m%d)"` builds that should be stable within a day. Each weighted feed now draws from its own stream, derived from the seed and its URL. The picks for a given seed therefore change once with this release.

A very large `--per-source` combined with a very large weight also no longer allocates memory in proportion to the weight.
