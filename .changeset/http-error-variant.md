---
"openring": patch
---

**fix**: say why a request failed, and replace `OpenringError::ReqwestError` with `OpenringError::HttpError`.

A failed fetch used to say only "error sending request for url (…)". The message now continues with the cause, such as "tcp connect error: Connection refused" or "dns error: failed to lookup address information". The library also no longer exposes `reqwest` types in its errors.
