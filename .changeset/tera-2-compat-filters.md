---
"openring": minor
---

**BREAKING CHANGE**: upgrade to tera 2.0, which changes the template language

Templates written for tera 1.x may need updating, since tera 2.0 removed or renamed several filters (`linebreaksbr` is now `newlines_to_br`, `filesizeformat` and `json_encode` are gone, and more; see the [tera migration guide](https://github.com/Keats/tera/blob/master/MIGRATION.md)). The `date`, `striptags`, `urlencode`, and `urlencode_strict` filters and the `now()` function moved out of tera core into tera-contrib, and openring registers all of them, so those keep working unchanged. The bundled example template is updated to the new syntax.
