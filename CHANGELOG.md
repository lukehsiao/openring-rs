## 0.1.12 - 2023-10-12

### Features
- Support feeds with relative URLs

See the commits here: [0.1.12]

[0.1.12]: https://github.com/lukehsiao/openring-rs/compare/v0.1.12...v0.1.12

## 0.1.11 - 2023-09-07

### Bug Fixes
- Log to stderr, not stdout

### Documentation
- (README) Fix grammar error
- (README) Suggest using `--locked` on install

### Refactor
- Standardize and clarify logs

See the commits here: [0.1.11]

[0.1.11]: https://github.com/lukehsiao/openring-rs/compare/v0.1.11...v0.1.11

## 0.1.10 - 2023-09-07

### Refactor
- Rename `--urls` to just `--url`
- Switch to `feed-rs`
    - **BREAKING**: We no longer can parse feeds whose dates do not include
      timezones. Support for this will need to be fixed upstream.

See the commits here: [0.1.10]

[0.1.10]: https://github.com/lukehsiao/openring-rs/compare/v0.1.10...v0.1.10

## 0.1.9 - 2023-08-11

### Features
- Provide `miette`-powered error diagnostics

See the commits here: [0.1.9]

[0.1.9]: https://github.com/lukehsiao/openring-rs/compare/v0.1.8...v0.1.9

## 0.1.8 - 2023-06-21

### Documentation
- (README) Add badges

### Miscellaneous Tasks
- Relicense to `BlueOak-1.0.0`

See the commits here: [0.1.8]

[0.1.8]: https://github.com/lukehsiao/openring-rs/compare/v0.1.7...v0.1.8

## 0.1.7 - 2023-05-21

### Build and Dependencies
- (deps) Update all dependencies
- (deps) Update all dependencies
- (deps) Bump serde from 1.0.162 to 1.0.163
- (deps) Bump clap from 4.2.7 to 4.3.0
- (deps) Update lockfile

### CI/CD
- Add basic workflow and dependabot config
- Check all targets/features

### Features
- Support naive datetime of form `%Y-%m-%dT%H:%M:%S`

### Refactor
- Use WarnLevel by default
- s/unable/failed/

See the commits here: [0.1.7]

[0.1.7]: https://github.com/lukehsiao/openring-rs/compare/v0.1.6...v0.1.7

## 0.1.6 - 2022-12-11

### Features
- Add `--before` to allow filtering to posts before a given date

See the commits here: [0.1.6]

[0.1.6]: https://github.com/lukehsiao/openring-rs/compare/v0.1.5...v0.1.6

## 0.1.5 - 2022-11-26

### Bug Fixes
- Trim whitespace around summaries

See the commits here: [0.1.5]

[0.1.5]: https://github.com/lukehsiao/openring-rs/compare/v0.1.4...v0.1.5

## 0.1.4 - 2022-11-26

### Bug Fixes
- Properly decode html entities

See the commits here: [0.1.4]

[0.1.4]: https://github.com/lukehsiao/openring-rs/compare/v0.1.3...v0.1.4

## 0.1.3 - 2022-11-26

### Bug Fixes
- Include the semicolon when stripping nbsp

See the commits here: [0.1.3]

[0.1.3]: https://github.com/lukehsiao/openring-rs/compare/v0.1.2...v0.1.3

## 0.1.2 - 2022-11-26

### Bug Fixes
- Strip non-breaking spaces from summary
- Use last link in atom entry for blogspot
- Default to using the alternate url

### Build and Dependencies
- Switch from log to tracing, add verbosity flag

### Documentation
- (README) Use unicode icon directly

See the commits here: [0.1.2]

[0.1.2]: https://github.com/lukehsiao/openring-rs/compare/v0.1.1...v0.1.2

## 0.1.1 - 2022-11-26

### Build and Dependencies
- (deps) Upgrade to clap v4
- (deps) Upgrade all deps

### Documentation
- (README) Add installation instructions
- (README) Use a fancier header

### Miscellaneous Tasks
- (contrib) Add release helper script

See the commits here: [0.1.1]

[0.1.1]: https://github.com/lukehsiao/openring-rs/compare/v0.1.0...v0.1.1
