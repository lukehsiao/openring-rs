---
"openring": patch
---

**refactor**: `run` now takes the `Terminal` that shows progress, and `progress::SuspendingStderr` is now `terminal::SuspendingStderr`, which `Terminal::log_writer` returns.

The binary builds one `Terminal` and shares it between the progress bars and the log writer, in place of a process-wide static. What the terminal shows is unchanged.
