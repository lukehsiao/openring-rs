---
"openring": patch
---

**fix**: name the file openring could not read.

A missing or unreadable template or urls file reported only "Failed to open file.", which did not say which file. The error now names the path. A failure to write the rendered output now says so instead of claiming a file failed to open. `OpenringError::IoError` is replaced by `ReadFileError` and `OutputError`.
