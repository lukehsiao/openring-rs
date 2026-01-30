# Changelog

All notable changes to this project will be documented in this file. See [conventional commits](https://www.conventionalcommits.org/) for commit guidelines.

---
## [0.5.0](https://github.com/lukehsiao/openring-rs/compare/v0.4.1..v0.5.0) - 2026-01-30

I'm once again changing the cache behavior.
Rather than storing caches per-project in `.openringcache`, we now store in OS-standard cache locations.

- **Linux**: `$XDG_CACHE_HOME/openring/cache.json` or `$HOME/.cache/openring/cache.json`
- **macOS**: `$HOME/Library/Caches/dev.hsiao.openring/cache.json`
- **Windows**: `{FOLDERID_LocalAppData}\hsiao\openring\cache\cache.json`

This way, the cache can easily benefit multiple sites, and you do not need to bother `.gitignore`-ing another file.

To that end, **caching is now the default behavior**.
It is the both the polite behavior (conditional requests, respecting 429s) and performant one, and as such, should be the default.
Now that we are not polluting the directory of invocation with a file, I feel confortable making this the default behavior.

If you were calling with `--cache` before, simply drop the argument.

### Features

- cache by default - ([809db64](https://github.com/lukehsiao/openring-rs/commit/809db64bcc403513f0852741dc514c9ee43a0d0f)) - Luke Hsiao
- move cache to standard project-based directories - ([a3b3008](https://github.com/lukehsiao/openring-rs/commit/a3b30088a1d642ed87304c2f676f65764aeb8b3d)) - Luke Hsiao

---
## [0.4.1](https://github.com/lukehsiao/openring-rs/compare/v0.4.0..v0.4.1) - 2026-01-29

Removes unused dependencies to reduce binary size slightly.

---
## [0.4.0](https://github.com/lukehsiao/openring-rs/compare/v0.3.12..v0.4.0) - 2026-01-29

Bumping the minor version here because we change the format of the cache file from CSV to JSON.
This shouldn't actually require any changes from any users, it will just behave as if you have no cache on first run.

### Performance

- also deduplicate in the parsing of the url file itself - ([0342255](https://github.com/lukehsiao/openring-rs/commit/03422554407ca0ad6d763d971057e8b8f5045df2)) - Luke Hsiao

### Refactor

- pull etag normalization into helper - ([2383e7e](https://github.com/lukehsiao/openring-rs/commit/2383e7e6ddc9287964bfafe419fb8010d728d00b)) - Luke Hsiao
- serde cache as json, not csv - ([a776733](https://github.com/lukehsiao/openring-rs/commit/a77673362342d0bec8f3d36f18db0fb956f4b62d)) - Luke Hsiao

---
## [0.3.12](https://github.com/lukehsiao/openring-rs/compare/v0.3.11..v0.3.12) - 2026-01-28

This release contains just a few small bugfixes as a result of me actually adding some testing.

### Bug Fixes

- **(cache)** clamp max_cache_age to the max value to prevent crashing - ([46ed04b](https://github.com/lukehsiao/openring-rs/commit/46ed04b34950e4585a41c3fd113cebf644c9fc21)) - Luke Hsiao
- **(lib)** correctly ignore whitespace in urlfile - ([43bf995](https://github.com/lukehsiao/openring-rs/commit/43bf995ec100987f0d3c781a81ab90d907dc08e5)) - Luke Hsiao

### Documentation

- **(README)** add roadmap for tests - ([3f23e95](https://github.com/lukehsiao/openring-rs/commit/3f23e950f873d301663f88e366170bc3c2e53be4)) - Luke Hsiao

---
## [0.3.11](https://github.com/lukehsiao/openring-rs/compare/v0.3.10..v0.3.11) - 2026-01-17

### Bug Fixes

- fall back to feed URL if feed itself has none - ([9260737](https://github.com/lukehsiao/openring-rs/commit/9260737541a32f2bd52b69ca08718aacbe86837f)) - Luke Hsiao

---
## [0.3.10](https://github.com/lukehsiao/openring-rs/compare/v0.3.9..v0.3.10) - 2026-01-01

### Build and Dependencies

- **(deps)** bump actions/checkout from 5 to 6 - ([a00c1dd](https://github.com/lukehsiao/openring-rs/commit/a00c1dde0f919bfd6d9e964822839e1f4846f549)) - dependabot[bot]
- **(deps)** bump tracing from 0.1.41 to 0.1.43 - ([b2d92a3](https://github.com/lukehsiao/openring-rs/commit/b2d92a32c90f070d95932e5778449f494f4e13a4)) - dependabot[bot]
- **(deps)** upgrade all dependencies - ([560b456](https://github.com/lukehsiao/openring-rs/commit/560b456f6220d3878c948c83eeefd429496a9c12)) - Luke Hsiao

---
## [0.3.9](https://github.com/lukehsiao/openring-rs/compare/v0.3.8..v0.3.9) - 2025-11-22

### Bug Fixes

- provide a more mobile-friendly template example - ([4a2c609](https://github.com/lukehsiao/openring-rs/commit/4a2c609401a6eee0ad2b1115e20fc6d645e0cccd)) - Luke Hsiao

### Documentation

- **(README)** minor capitalization tweaks - ([b349b1a](https://github.com/lukehsiao/openring-rs/commit/b349b1a31e608597372ee39515db67b16b308a34)) - Luke Hsiao

---
## [0.3.8](https://github.com/lukehsiao/openring-rs/compare/v0.3.7..v0.3.8) - 2025-07-21

### Build and Dependencies

- **(deps)** update all dependencies (namely, indicatif, which yanked a version which causes builds to fail) - ([cca0751](https://github.com/lukehsiao/openring-rs/commit/cca0751869a2ba26919dba45bd220c76e02ee153)) - Luke Hsiao

---
## [0.3.7](https://github.com/lukehsiao/openring-rs/compare/v0.3.6..v0.3.7) - 2025-03-22

### Documentation

- **(LICENSE)** use markdown for nicer rendering - ([0cfa7a1](https://github.com/lukehsiao/openring-rs/commit/0cfa7a11d2921a3130d5dae737d50455a7274683)) - Luke Hsiao

### Build and Dependencies

- **(Justfile)** use pedantic on `check` - ([071c354](https://github.com/lukehsiao/openring-rs/commit/071c3548444a29328580a1c91fa7eb02c14628d0)) - Luke Hsiao
- **(deps)** bump serde from 1.0.210 to 1.0.214 - ([aaa343e](https://github.com/lukehsiao/openring-rs/commit/aaa343e8a5c547a086725d3bd06c3f0207a92a39)) - dependabot[bot]
- **(deps)** bump thiserror from 1.0.64 to 1.0.66 - ([2d41eed](https://github.com/lukehsiao/openring-rs/commit/2d41eedaa4985e475be62e6e99168520536f12ad)) - dependabot[bot]
- **(deps)** bump reqwest from 0.12.8 to 0.12.9 - ([322431d](https://github.com/lukehsiao/openring-rs/commit/322431d12a97c79c9e0845567e63081a0c73df3e)) - dependabot[bot]
- **(deps)** bump tokio from 1.40.0 to 1.41.0 - ([5231c0e](https://github.com/lukehsiao/openring-rs/commit/5231c0e33b6cda5b2ce3a2aae5355ad9ef421e64)) - dependabot[bot]
- **(deps)** bump jiff from 0.1.13 to 0.1.14 - ([293d0ab](https://github.com/lukehsiao/openring-rs/commit/293d0abf1036cf23cb18a798d137d7a2f4049d2a)) - dependabot[bot]
- **(deps)** bump thiserror from 1.0.69 to 2.0.3 - ([caa602a](https://github.com/lukehsiao/openring-rs/commit/caa602a5b00c28de36d7f98adefb5b68076e9f72)) - Luke Hsiao
- **(deps)** upgrade all dependencies - ([64e6a57](https://github.com/lukehsiao/openring-rs/commit/64e6a5720dd65563980d781cb24712d9fcb7f7d3)) - Luke Hsiao
- **(deps)** bump thiserror from 2.0.3 to 2.0.11 - ([8dbf8dc](https://github.com/lukehsiao/openring-rs/commit/8dbf8dcc86a85d4b1671dfead4f06d746bbe2b27)) - dependabot[bot]
- **(deps)** bump clap from 4.5.21 to 4.5.27 - ([2391470](https://github.com/lukehsiao/openring-rs/commit/2391470c8cf3c83ee3db8d129c0b8f4eba1e671d)) - dependabot[bot]
- **(deps)** update all dependencies - ([95d1851](https://github.com/lukehsiao/openring-rs/commit/95d185195279989bd294ae37639a18752a5d24b6)) - Luke Hsiao
- **(deps)** update lockfile - ([4833d69](https://github.com/lukehsiao/openring-rs/commit/4833d693bede16f0358920449f58bc7192ee937f)) - Luke Hsiao
- **(deps)** update all dependencies - ([3195e35](https://github.com/lukehsiao/openring-rs/commit/3195e35a6f425f2366007a108c2f75fd4878c036)) - Luke Hsiao
- bump to rust 2024 edition - ([fdf1c40](https://github.com/lukehsiao/openring-rs/commit/fdf1c40123cf44f83c6eb3201005689ab8cb6c09)) - Luke Hsiao

---
## [0.3.6](https://github.com/lukehsiao/openring-rs/compare/v0.3.5..v0.3.6) - 2024-10-11

This release further adopts `cargo`-style progress, by showing all the remaining URLs in the message, rather than just the most recently fetched URL.

### Features

- show all remaining URLs in progress bar - ([752a433](https://github.com/lukehsiao/openring-rs/commit/752a43387a77d452d0d2bfc209547b2c28bf0c07)) - Luke Hsiao

### Refactor

- use `wide_msg` in progress for auto truncation - ([a237d87](https://github.com/lukehsiao/openring-rs/commit/a237d876773962fbc1a34039aa80981b0824e062)) - Luke Hsiao
- simplify main by using `tracing_log` - ([b7096b1](https://github.com/lukehsiao/openring-rs/commit/b7096b1eb12846447ffc68cc928bc57a524e0bc8)) - Luke Hsiao

---
## [0.3.5](https://github.com/lukehsiao/openring-rs/compare/v0.3.4..v0.3.5) - 2024-10-09

### Bug Fixes

- support compressed feeds (gzip, ztd, brotli, deflate) - ([2d3e467](https://github.com/lukehsiao/openring-rs/commit/2d3e4671d824d02ad787f340de58d16a2648c346)) - Luke Hsiao

---
## [0.3.4](https://github.com/lukehsiao/openring-rs/compare/v0.3.3..v0.3.4) - 2024-10-09

The primary change of this release is changing to `cargo`-style progress.
It's subjectively a little more explicit and clear.

### Documentation

- **(README)** update Tera link and call out up front - ([b5ac27d](https://github.com/lukehsiao/openring-rs/commit/b5ac27d5c35944c12dddd67922bbaca60c1324a3)) - Luke Hsiao

### Refactor

- simplify progress bar - ([ee98561](https://github.com/lukehsiao/openring-rs/commit/ee98561b4f006085637832cfff080406d8d2fee7)) - Luke Hsiao
- switch to cargo-style progress - ([f1db309](https://github.com/lukehsiao/openring-rs/commit/f1db30995a0df6870c24d3bedc0b9052150d0b1f)) - Luke Hsiao
- print error progress correctly - ([dcd0b6b](https://github.com/lukehsiao/openring-rs/commit/dcd0b6b7b46005994b5d49525b6af35995293c17)) - Luke Hsiao

### Build and Dependencies

- **(deps)** bump clap from 4.5.18 to 4.5.19 - ([8c9431e](https://github.com/lukehsiao/openring-rs/commit/8c9431e4f359191f3bb67394157add536a5b1f66)) - dependabot[bot]
- **(deps)** bump reqwest from 0.12.7 to 0.12.8 - ([2f3484f](https://github.com/lukehsiao/openring-rs/commit/2f3484fb2fd1ba1eadccce2cc97b92b71439d28a)) - dependabot[bot]
- **(deps)** bump all dependencies - ([02c70b5](https://github.com/lukehsiao/openring-rs/commit/02c70b5d197328fd87c1efa83d9dc2f4b76c6cae)) - Luke Hsiao

---
## [0.3.2](https://github.com/lukehsiao/openring-rs/compare/v0.2.5..v0.3.2) - 2024-09-29

This release is a significant internal refactor that improves performance by fetching _all_ feeds concurrently.
In addition, we also deduplicate feeds to avoid unnecessary fetching.

### Bug Fixes

- do not hold dashmap lock over await call - ([bd706ba](https://github.com/lukehsiao/openring-rs/commit/bd706ba44c7ec15af537477a770024b827448e03)) - Luke Hsiao

### Performance

- switch from `rayon` to `tokio` - ([64a354d](https://github.com/lukehsiao/openring-rs/commit/64a354de873cf06617a81e99c50f3cf701a3c9db)) - Luke Hsiao
- deduplicate feed urls - ([e590e4d](https://github.com/lukehsiao/openring-rs/commit/e590e4d5940a39a201d69dc515e6837d071523cc)) - Luke Hsiao

---
## [0.2.5](https://github.com/lukehsiao/openring-rs/compare/v0.2.4..v0.2.5) - 2024-09-28

### Bug Fixes

- increase timeout from 10s to 30s - ([20f9c55](https://github.com/lukehsiao/openring-rs/commit/20f9c55c69e054d00423c9bbbddce4fe8cd7f3f6)) - Luke Hsiao

### Documentation

- **(README)** add roadmap - ([319c891](https://github.com/lukehsiao/openring-rs/commit/319c8910d72a1b32d55929bf2e250808d746fd34)) - Luke Hsiao

### Refactor

- split into modules - ([fc5c9a5](https://github.com/lukehsiao/openring-rs/commit/fc5c9a50f0b28edabb0b8f6c8a88e481983960b1)) - Luke Hsiao
- pull feed fetching logic into a trait - ([c17f57a](https://github.com/lukehsiao/openring-rs/commit/c17f57aa38273af8b16611ef3234d0a495b58154)) - Luke Hsiao

---
## [0.2.4](https://github.com/lukehsiao/openring-rs/compare/v0.2.3..v0.2.4) - 2024-08-14

### Bug Fixes

- default to 4hrs when receiving a 429 - ([351d563](https://github.com/lukehsiao/openring-rs/commit/351d563efd5c556517ac99d0fdb37e0b5034323b)) - Luke Hsiao

### Documentation

- **(README)** add link to demo of the webring - ([09e1b3c](https://github.com/lukehsiao/openring-rs/commit/09e1b3c58726b3c3345c8bced1bf825174dd4a71)) - Luke Hsiao

---
## [0.2.3](https://github.com/lukehsiao/openring-rs/compare/v0.2.2..v0.2.3) - 2024-08-08

### Bug Fixes

- adjust log levels - ([9c86048](https://github.com/lukehsiao/openring-rs/commit/9c860488cd4a555867bfe95cafc61b53cfd62d5e)) - Luke Hsiao

---
## [0.2.2](https://github.com/lukehsiao/openring-rs/compare/v0.2.1..v0.2.2) - 2024-08-08

Minor release that now allows feed entries without summary/content.

### Bug Fixes

- allow entries with no summary/content - ([02bcde3](https://github.com/lukehsiao/openring-rs/commit/02bcde3d21ac263ada6f9c97bf28be8faa909562)) - Luke Hsiao

---
## [0.2.1](https://github.com/lukehsiao/openring-rs/compare/v0.2.0..v0.2.1) - 2024-08-08

**This release adds a nice quality of life feature: local caching.**

We want to respect `Etag` and `Last-Modified` headers when sending requests to reduce resource strain on the servers providing feeds.
Similarly, we want to respect `Retry-After` if a server provides that header when responding with an HTTP 429.

This patch respects both by introducing a local cache option in `.openringcache`, which is a simple CSV file with the schema: url, timestamp, retry_after, last_modified, etag, and body, where body is the entire content of the response body last time we fetched the feed.

With this local cache, if we have a value for Retry-After, we know we were throttled, so we skip sending a request and just use the feed from the cache.

Otherwise, if we have a cache value, we send a conditional request, setting `If-Modified-Since` and `Etag` headers in the request.

If we don't have a cache value, we send an unconditional request.

### Features

- add caching options to respect headers - ([0b51bc9](https://github.com/lukehsiao/openring-rs/commit/0b51bc93a0b73a00cbc8c5220cffcc02a92f89ea)) - Luke Hsiao

### Build and Dependencies

- **(deps)** bump clap from 4.5.11 to 4.5.13 - ([2896fc0](https://github.com/lukehsiao/openring-rs/commit/2896fc02e6a8c9710812ea0b9e2c2e6682c7cd64)) - dependabot[bot]
- **(deps)** bump jiff from 0.1.2 to 0.1.3 - ([f0927d2](https://github.com/lukehsiao/openring-rs/commit/f0927d2b2f2429233a8edcc97ff789e33c65074d)) - dependabot[bot]
- **(deps)** bump serde_json from 1.0.121 to 1.0.122 - ([2b5ec04](https://github.com/lukehsiao/openring-rs/commit/2b5ec04296851c77b09cf074733b9faec288d666)) - dependabot[bot]
- **(deps)** upgrade all dependencies - ([32248c0](https://github.com/lukehsiao/openring-rs/commit/32248c0c4059f0eb91947c7129243371d80e50f6)) - Luke Hsiao
- tweak changelog and order of release checks - ([c8c6ebe](https://github.com/lukehsiao/openring-rs/commit/c8c6ebeb25f9f15a4fdeac437a3b2804c92e00b2)) - Luke Hsiao

---
## [0.2.0](https://github.com/lukehsiao/openring-rs/compare/v0.1.15..v0.2.0) - 2024-07-29

In this release, the only meaningful change is changing from `chrono` to `jiff` as a dependency.
However, this does also rename `article.date` to `article.timestamp` to better reflect reality.
It is likely you will simply need to update your template to `s/article.date/article.timestamp/` and be on your way.

### Build and Dependencies

- **(deps)** [**breaking**] switch from `chrono` to `jiff` - ([485fe4e](https://github.com/lukehsiao/openring-rs/commit/485fe4ef480c9f08a1c895b9e6b75b8c2b6f3774)) - Luke Hsiao

---
## [0.1.15](https://github.com/lukehsiao/openring-rs/compare/v0.1.14..v0.1.15) - 2024-06-04

### Dependencies
- Bump to `feed-rs` v2.0.0 - Luke Hsiao

### Styling

- **(README)** 1 sentence per line and consistent indentation - ([04bbb05](https://github.com/lukehsiao/openring-rs/commit/04bbb05abfc296d52963bdf8e36dcbbe6ecc1b98)) - Luke Hsiao
- run rustfmt - ([25413ff](https://github.com/lukehsiao/openring-rs/commit/25413ffb423b7f7bc6d22bd61c4af5b6e97da121)) - Luke Hsiao

---
## [0.1.14](https://github.com/lukehsiao/openring-rs/compare/v0.1.13..v0.1.14) - 2024-01-18

### Documentation

- **(CHANGELOG)** add entry for v0.1.14 - ([487c784](https://github.com/lukehsiao/openring-rs/commit/487c784e84af5c34da6d680625187328a0f101f1)) - Luke Hsiao
- **(README)** link license badge to license - ([0ce9e45](https://github.com/lukehsiao/openring-rs/commit/0ce9e4547946d7fa1aa931adae7d950b4f4a6f7f)) - Luke Hsiao

### Refactor

- default to error-level logs - ([23e355a](https://github.com/lukehsiao/openring-rs/commit/23e355a3fdc39f4e10bc496458b6588e20fb7b85)) - Luke Hsiao

---
## [0.1.13](https://github.com/lukehsiao/openring-rs/compare/v0.1.12..v0.1.13) - 2023-10-12

### Bug Fixes

- make relative urls relative to origin - ([a73455c](https://github.com/lukehsiao/openring-rs/commit/a73455cb14831c3834d8c538949bf698b8296b7b)) - Luke Hsiao
- ignore "self" rel on links - ([5968eda](https://github.com/lukehsiao/openring-rs/commit/5968eda6c7e52071bd871abcc68720c0a00704d1)) - Luke Hsiao

### Documentation

- **(CHANGELOG)** add entry for v0.1.13 - ([64e72e0](https://github.com/lukehsiao/openring-rs/commit/64e72e02847792223c5655dc8c9fbbe547270124)) - Luke Hsiao

### Features

- default to domain name if feed title is empty - ([1b08b27](https://github.com/lukehsiao/openring-rs/commit/1b08b27df8f0f9bbb1ae8284cf0b397e36b00614)) - Luke Hsiao

---
## [0.1.12](https://github.com/lukehsiao/openring-rs/compare/v0.1.11..v0.1.12) - 2023-10-12

### Documentation

- **(CHANGELOG)** add entry for v0.1.12 - ([900117e](https://github.com/lukehsiao/openring-rs/commit/900117e47fd0db76af956c97765ecd15aac0e35c)) - Luke Hsiao

### Features

- support feeds with relative URLs - ([f85009b](https://github.com/lukehsiao/openring-rs/commit/f85009b14763098692bb682e2c51f6bcd9f8b5b3)) - Luke Hsiao

---
## [0.1.11](https://github.com/lukehsiao/openring-rs/compare/v0.1.10..v0.1.11) - 2023-09-07

### Bug Fixes

- log to stderr, not stdout - ([9e8a2d6](https://github.com/lukehsiao/openring-rs/commit/9e8a2d6775dd7f41b067c7b59b56e8ec8ffb0241)) - Luke Hsiao

### Documentation

- **(CHANGELOG)** add entry for v0.1.11 - ([4efddbf](https://github.com/lukehsiao/openring-rs/commit/4efddbf5ff73e8343a663a03b7e8fa07a7de2dea)) - Luke Hsiao
- **(README)** fix grammar error - ([4d1e778](https://github.com/lukehsiao/openring-rs/commit/4d1e7785e7b0bf23910b94d321fd519532f48015)) - Luke Hsiao
- **(README)** suggest using `--locked` on install - ([445c6d6](https://github.com/lukehsiao/openring-rs/commit/445c6d6df40356bcc86de7637aa98bafe825f42d)) - Luke Hsiao

### Refactor

- standardize and clarify logs - ([64da97b](https://github.com/lukehsiao/openring-rs/commit/64da97bf91a1d9abdd73d7fdf09847461dbba48d)) - Luke Hsiao

---
## [0.1.10](https://github.com/lukehsiao/openring-rs/compare/v0.1.9..v0.1.10) - 2023-09-07

### Documentation

- **(CHANGELOG)** add entry for v0.1.10 - ([435f16c](https://github.com/lukehsiao/openring-rs/commit/435f16c2a647894b23f246bea70b8cdea8a38fb7)) - Luke Hsiao

### Refactor

- rename `--urls` to just `--url` - ([178788b](https://github.com/lukehsiao/openring-rs/commit/178788b37e05dbd8db6c2f371473dba0ae4cb739)) - Luke Hsiao
-  [**breaking**] switch to `feed-rs` - ([032add1](https://github.com/lukehsiao/openring-rs/commit/032add1034cfc72786957a34b1705606fd1f6488)) - Luke Hsiao

---
## [0.1.9](https://github.com/lukehsiao/openring-rs/compare/v0.1.8..v0.1.9) - 2023-08-11

### Documentation

- **(CHANGELOG)** add entry for v0.1.9 - ([8e694d0](https://github.com/lukehsiao/openring-rs/commit/8e694d063d1be2cb73e5fabe6b72f23c836d94ee)) - Luke Hsiao

### Features

- provide `miette`-powered error diagnostics - ([88c63a0](https://github.com/lukehsiao/openring-rs/commit/88c63a00fc0c28cedbb77a7debcd2d49c728419c)) - Luke Hsiao

---
## [0.1.8](https://github.com/lukehsiao/openring-rs/compare/v0.1.7..v0.1.8) - 2023-06-21

### Documentation

- **(CHANGELOG)** add entry for v0.1.8 - ([cd9ed3c](https://github.com/lukehsiao/openring-rs/commit/cd9ed3c97d40a82356f3cec19c2c3a89c6eb31ca)) - Luke Hsiao
- **(README)** add badges - ([5334775](https://github.com/lukehsiao/openring-rs/commit/5334775b30468df49ae0d9b6e56c109b590b4e47)) - Luke Hsiao

---
## [0.1.7](https://github.com/lukehsiao/openring-rs/compare/v0.1.6..v0.1.7) - 2023-05-21

### Documentation

- **(CHANGELOG)** add entry for v0.1.7 - ([231a812](https://github.com/lukehsiao/openring-rs/commit/231a812f0de14e1a6bbcb5153c7bc5cbca6de3fd)) - Luke Hsiao

### Features

- support naive datetime of form `%Y-%m-%dT%H:%M:%S` - ([a1e2d4d](https://github.com/lukehsiao/openring-rs/commit/a1e2d4dd8698bad6a09086cbf787e0d370180e75)) - Luke Hsiao

### Refactor

- use WarnLevel by default - ([7bf66d6](https://github.com/lukehsiao/openring-rs/commit/7bf66d6cdc0c313d1b60ee7462fa6cd12fafbfc6)) - Luke Hsiao
- s/unable/failed/ - ([308d08f](https://github.com/lukehsiao/openring-rs/commit/308d08fc790f6412768bcff4022ff860a5cf5f12)) - Luke Hsiao

---
## [0.1.6](https://github.com/lukehsiao/openring-rs/compare/v0.1.5..v0.1.6) - 2022-12-11

### Documentation

- **(CHANGELOG)** add entry for v0.1.6 - ([811582c](https://github.com/lukehsiao/openring-rs/commit/811582c38b254e62266d5c18f739e68de0ac1c73)) - Luke Hsiao

### Features

- add `--before` to allow filtering to posts before a given date - ([4d42a33](https://github.com/lukehsiao/openring-rs/commit/4d42a33202bcb784216f75fb03f94d63a48ec540)) - Luke Hsiao

---
## [0.1.5](https://github.com/lukehsiao/openring-rs/compare/v0.1.4..v0.1.5) - 2022-11-26

### Bug Fixes

- trim whitespace around summaries - ([14e37d7](https://github.com/lukehsiao/openring-rs/commit/14e37d7f1b3bf58394f3eb51b2f8e38cd10fb561)) - Luke Hsiao

### Documentation

- **(CHANGELOG)** add entry for v0.1.5 - ([e56d89f](https://github.com/lukehsiao/openring-rs/commit/e56d89f97009727548b53f84f1e99e512d8b784d)) - Luke Hsiao

---
## [0.1.4](https://github.com/lukehsiao/openring-rs/compare/v0.1.3..v0.1.4) - 2022-11-26

### Bug Fixes

- properly decode html entities - ([3ddbcd8](https://github.com/lukehsiao/openring-rs/commit/3ddbcd801166c9b33385027a7042c66971ca899a)) - Luke Hsiao

### Documentation

- **(CHANGELOG)** add entry for v0.1.4 - ([8c9290a](https://github.com/lukehsiao/openring-rs/commit/8c9290a3f14403bc68ae20acc4c091dcf061e372)) - Luke Hsiao

---
## [0.1.3](https://github.com/lukehsiao/openring-rs/compare/v0.1.2..v0.1.3) - 2022-11-26

### Bug Fixes

- include the semicolon when stripping nbsp - ([d9b9fd4](https://github.com/lukehsiao/openring-rs/commit/d9b9fd42ec6120d4dbf9689268f53c072e59d18d)) - Luke Hsiao

### Documentation

- **(CHANGELOG)** add entry for v0.1.3 - ([c9dda71](https://github.com/lukehsiao/openring-rs/commit/c9dda71b9f7a798be65399787cf9c6d738eeaa22)) - Luke Hsiao

---
## [0.1.2](https://github.com/lukehsiao/openring-rs/compare/v0.1.1..v0.1.2) - 2022-11-26

### Bug Fixes

- strip non-breaking spaces from summary - ([e196901](https://github.com/lukehsiao/openring-rs/commit/e19690173dc5d57ebac0b791fa29d15932ca8f7b)) - Luke Hsiao
- use last link in atom entry for blogspot - ([d169aef](https://github.com/lukehsiao/openring-rs/commit/d169aef6858c0307498ca5963528e2de5a3e4f97)) - Luke Hsiao
- default to using the alternate url - ([f314f02](https://github.com/lukehsiao/openring-rs/commit/f314f02382d7bed889ce36897c3184b6a22d7a5e)) - Luke Hsiao

### Documentation

- **(CHANGELOG)** add entry for v0.1.2 - ([aab19db](https://github.com/lukehsiao/openring-rs/commit/aab19db19683218ca41790ce6e0e4eae80b48d32)) - Luke Hsiao
- **(README)** use unicode icon directly - ([5de0aef](https://github.com/lukehsiao/openring-rs/commit/5de0aefaf60622d689aa387c39c0bfa56e657584)) - Luke Hsiao

---
## [0.1.1](https://github.com/lukehsiao/openring-rs/compare/v0.1.0..v0.1.1) - 2022-11-26

### Documentation

- **(CHANGELOG)** add entry for v0.1.1 - ([1052862](https://github.com/lukehsiao/openring-rs/commit/1052862697e4891843b8ee75f101a5e19ad016a3)) - Luke Hsiao
- **(README)** add installation instructions - ([9d41547](https://github.com/lukehsiao/openring-rs/commit/9d41547c5ca890256da0f87e0f3540738570307f)) - Luke Hsiao
- **(README)** use a fancier header - ([3e171a7](https://github.com/lukehsiao/openring-rs/commit/3e171a7bd3a4252bcf45b42af724c5101347a9fc)) - Luke Hsiao

---
## [0.1.0] - 2022-09-17

### Bug Fixes

- switch to fixedoffset and support more date formats - ([c673d77](https://github.com/lukehsiao/openring-rs/commit/c673d774ee1d2aa84205f340cf08cf6511fb9ebd)) - Luke Hsiao

### Documentation

- **(README)** add initial README - ([ab76822](https://github.com/lukehsiao/openring-rs/commit/ab76822ccf256d39089b39ea39ce518c5c66b035)) - Luke Hsiao
- **(README)** update option help messages - ([94451bd](https://github.com/lukehsiao/openring-rs/commit/94451bd2bd53c1854a00ab8612eba131128742e9)) - Luke Hsiao

### Features

- finish initial implementation - ([fc58c31](https://github.com/lukehsiao/openring-rs/commit/fc58c312d2d1e0f7281d24d026b8a30bb8b69512)) - Luke Hsiao
- provide basic progress bar with indicatif - ([f1efb04](https://github.com/lukehsiao/openring-rs/commit/f1efb0458d5057aa03728c96c5b502795b8fea63)) - Luke Hsiao
- show actual urls with indicatif progress - ([eb54b01](https://github.com/lukehsiao/openring-rs/commit/eb54b018e139690d320c0b785a81db3a93acc1b0)) - Luke Hsiao

### Performance

- parallelize requests with rayon - ([7169222](https://github.com/lukehsiao/openring-rs/commit/716922207826c341715f0d9b9f9a3e46811a52de)) - Luke Hsiao

### Refactor

- add basic arguments to match openring - ([b5dec8b](https://github.com/lukehsiao/openring-rs/commit/b5dec8b29a1d277e16ccb2e1228a442cc7239171)) - Luke Hsiao
- add basic logging and anyhow - ([a3dd708](https://github.com/lukehsiao/openring-rs/commit/a3dd708aca4716976ab28cd34a10a49737e10a3c)) - Luke Hsiao
- move core impl into lib.rs - ([2cbf54a](https://github.com/lukehsiao/openring-rs/commit/2cbf54a4e1fa143a887da5fd628bba7e01b5fa0d)) - Luke Hsiao
- allow parsing a url file - ([3a2374d](https://github.com/lukehsiao/openring-rs/commit/3a2374dd2238b1e8e02b2cccd66f05fe8b6d2aa4)) - Luke Hsiao
- setup structure for tera - ([435b181](https://github.com/lukehsiao/openring-rs/commit/435b181c044e9f69ee74afa9d136dc4f98ac28a5)) - Luke Hsiao
- error if no feed urls are provided - ([65393eb](https://github.com/lukehsiao/openring-rs/commit/65393eb0c96d203b6b459667e067feb3594cf247)) - Luke Hsiao

