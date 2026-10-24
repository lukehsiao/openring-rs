---
"openring": patch
---

**fix**: say "Failed to render the template." when a template fails while rendering.

A template that parsed but then failed, for example on a variable that does not exist, reported "Failed to parse tera template." It now reports a render failure with the `openring::render_error` code, and Tera's own message still follows. A template that does not parse now reads "Failed to parse the template." `OpenringError` gains a `RenderError` variant, and `TemplateError` no longer converts from every `tera::Error`.
