---
"openring": patch
---

**fix**: upgrade `webpki-root-certs` to 1.0.10 to drop the distrusted `emSign ECC Root CA - G3` root ([RUSTSEC-2026-0383](https://rustsec.org/advisories/RUSTSEC-2026-0383)).

The private keys of eight eMudhra intermediates under emSign G3 roots were compromised ([Mozilla bug 2083449](https://bugzilla.mozilla.org/show_bug.cgi?id=2083449)). Only `wasm32` builds use this bundled root store. Native builds verify through the OS via `rustls-platform-verifier`, so their exposure depends on the OS trust and revocation policy. Windows and macOS apply the OS distrust and revocation decisions. Linux and BSD only trust the system CA bundle and do no revocation checks, so they need an updated CA bundle.

All other dependencies are also upgraded to their latest versions, including `html-escape` 0.3.0.
