---
"openring": patch
---

**fix**: stop timing out large feeds on slow servers.

The 30-second timeout was a total deadline covering the entire transfer, so a perfectly healthy feed that is just large and slow (a 1.7 MiB feed at ~85 KiB/s) failed midway through its body, especially with many feeds sharing bandwidth.
Timeouts are now granular: 10 seconds to connect, 30 seconds of complete silence to declare a stall, and a 5-minute overall ceiling so a trickling server cannot hold a fetch slot forever.
A transfer that keeps flowing is allowed to finish.
