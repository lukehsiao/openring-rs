---
"openring": patch
---

**fix**: don't truncate the cache file before holding its lock.

The cache writer truncated the file first and acquired the exclusive lock second, so a concurrent openring run reading the cache under its shared lock could see the data vanish mid-read.
The file is now emptied only after the lock is held, and the write buffer is flushed explicitly so disk errors surface instead of being silently dropped.
