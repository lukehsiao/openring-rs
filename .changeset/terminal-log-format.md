---
"openring": patch
---

**fix**: shorten log lines in a terminal and drop color codes outside one.

In a terminal, warnings and debug lines no longer carry a timestamp or module path, so they stop wrapping beside the progress list. `NO_COLOR` still turns color off. Outside a terminal, for example in cron or CI logs, lines keep their timestamps but no longer contain ANSI escape codes.
