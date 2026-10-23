---
"openring": patch
---

**fix**: report a feed that fails to fetch as a warning when stderr is not a terminal.

Before, the failure only appeared as an `Error` line on the progress bar. The bar is hidden outside a terminal, so cron jobs and CI site builds dropped failed feeds without a word, even with `-vvv`. They now get a warning at the default verbosity, which `-q` silences. In a terminal, the progress bar's `Error` line is unchanged.
