---
"openring": patch
---

**fix**: stop pinning the AUR `openring-rs-bin` package to GitHub's auto-generated `archive/` tarball.

Release archives now carry `LICENSE.md` and `README.md` alongside the binary, so the `-bin` PKGBUILD no longer needs the `archive/` tarball, whose bytes are not stable over time and could have started failing checksum validation without any release changing.
