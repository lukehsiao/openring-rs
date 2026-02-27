# just manual: https://github.com/casey/just

_default:
    @just --list

# Runs clippy on the sources
[group('dev')]
check:
    cargo clippy --all-targets --all-features --locked -- -W clippy::pedantic -D warnings

# Runs the test suite
[group('dev')]
test:
    cargo nextest run

# Runs the test suite to compute coverage
[group('dev')]
coverage *FLAGS:
    cargo llvm-cov nextest {{FLAGS}}

# check security advisories
[group('dev')]
audit:
    cargo deny check advisories

# Check links in markdown files
[group('dev')]
link-check:
    -lychee -E '**/*.md'

# Format source
[group('dev')]
fmt:
    cargo fmt

# Sets up a watcher that lints, tests, and builds
[group('dev')]
watch:
    bacon

# Update all dependencies
[group('build')]
upgrade:
    pnpm up --recursive
    pnpm install
    cargo upgrade
    cargo update

# Install release tooling
[group('build')]
install:
    pnpm install

# Interactively create a changeset.
[group('release')]
changeset *args:
    pnpm changeset {{ args }}

# Sync version from package.json to Cargo manifest
_sync-versions:
    #!/usr/bin/env bash
    set -euxo pipefail

    # read version from package.json
    version=$(jaq -r '.version' package.json)

    # ensure we found a version
    [ -n "$version" ]
    # replace a version line that starts at column 1: version = "..."
    sd '^version\s+=\s+".*"$' "version = \"$version\"" Cargo.toml
    echo "Cargo.toml version set to $version"	

# Create a version bump
[group('release')]
version *args:
    pnpm changeset version {{ args }}
    just _sync-versions

# Publish a new version on crates.io
[group('release')]
publish:
    pnpm changeset publish
    cargo publish

# Show pending changesets and expected version bumps.
[group('release')]
status *args:
    pnpm changeset status {{ args }}
