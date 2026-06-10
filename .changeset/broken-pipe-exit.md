---
"openring": patch
---

**fix**: exit cleanly when stdout closes early instead of panicking.

Piping output into a tool that stops reading, like `openring ... | head`, used to panic with a broken-pipe error.
A closed stdout is now treated as success, so openring exits quietly the way Unix tools are expected to.
