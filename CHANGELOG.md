# Changelog

All notable changes to this project will be documented in this file. See [conventional commits](https://www.conventionalcommits.org/) for commit guidelines.

---
## [0.1.14](https://github.com/lukehsiao/openring-rs/compare/v0.1.13..0.1.14) - 2024-01-18

### Documentation

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
-  [**breaking**]switch to `feed-rs` - ([032add1](https://github.com/lukehsiao/openring-rs/commit/032add1034cfc72786957a34b1705606fd1f6488)) - Luke Hsiao

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
