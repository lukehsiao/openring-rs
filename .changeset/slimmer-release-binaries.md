---
"openring": patch
---

**perf**: shrink release binaries from 20M to 15M

The release profile now strips the symbol table and enables fat LTO with a single codegen unit. No behavior change, and prebuilt binaries from GitHub releases get the reduction automatically.
