<h1 align="center">
    ⛓<br>
    openring-rs
</h1>
<div align="center">
    <strong>A tool for generating a webring from Atom/RSS feeds.</strong>
</div>
<br>
<div align="center">
  <a href="https://github.com/lukehsiao/openring-rs/actions/workflows/general.yml">
    <img src="https://img.shields.io/github/actions/workflow/status/lukehsiao/openring-rs/general.yml" alt="Build Status">
  </a>
  <a href="https://crates.io/crates/openring">
    <img src="https://img.shields.io/crates/v/openring" alt="Version">
  </a>
  <a href="https://github.com/lukehsiao/openring-rs/blob/main/LICENSE.md">
    <img src="https://img.shields.io/crates/l/openring" alt="License">
  </a>
</div>
<br>

`openring-rs` is a tool for generating a webring from Atom/RSS feeds, so you can populate a template with articles from those feeds and embed them in your own blog. An example template is provided in `in.html`.

This is a Rust-port of Drew DeVault's [openring](https://git.sr.ht/~sircmpwn/openring), with the primary differences being:
- we respect throttling and send conditional requests by default via caching (disable with `--no-cache`)
- the template is written using [Tera](https://keats.github.io/tera/) and is provided as an argument, not read from stdin
- we show a little progress bar
- we fetch all feeds concurrently
- we provide better error messages (via [miette](https://github.com/zkat/miette))
- we allow filtering feeds with `--before`

## Demo

To see this in action, you can look at the footer of this blog post.

<https://luke.hsiao.dev/blog/openring-rs/>

## Install

```
cargo install --locked openring
```

Or, if you use [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall):

```
cargo binstall openring
```

## Usage

```
A webring for static site generators written in Rust

Usage: openring [OPTIONS] --template-file <FILE>

Options:
  -n, --num-articles <NUM_ARTICLES>    Total number of articles to fetch [default: 3]
  -p, --per-source <PER_SOURCE>        Number of most recent articles to get from each feed [default: 1]
  -S, --url-file <FILE>                File with URLs of Atom/RSS feeds to read (one URL per line, lines starting with '#' or "//" are ignored)
  -t, --template-file <FILE>           Tera template file
  -s, --url <URL>                      A single URL to consider (can be repeated to specify multiple)
  -b, --before <BEFORE>                Only include articles before this date (in YYYY-MM-DD format)
      --no-cache                       Do NOT use request cache stored on disk
      --max-cache-age <MAX_CACHE_AGE>  Discard all cached requests older than this duration [default: 30d]
  -v, --verbose...                     Increase logging verbosity
  -q, --quiet...                       Decrease logging verbosity
  -h, --help                           Print help (see more with '--help')
  -V, --version                        Print version
```

## Using Tera templates

The templates supported by `openring-rs` are written using [Tera](https://keats.github.io/tera/).
Please refer to the Tera documentation for details.

## Caching

We use OS-standard locations for caching.

- **Linux**: `$XDG_CACHE_HOME/openring/cache.json` or `$HOME/.cache/openring/cache.json`
- **macOS**: `$HOME/Library/Caches/dev.hsiao.openring/cache.json`
- **Windows**: `{FOLDERID_LocalAppData}\hsiao\openring\cache\cache.json`

The cache file is simple JSON.

The cache only prevents refetching a feed if the feed source responds with a 429.
In this case, we respect `Retry-After`, or default to 4 hours.
Otherwise, we use the cache to send conditional requests by respecting the `ETag` and `Last-Modified` headers.

## Why a Rust port?

Just for fun.

## TODO

### Test suite
I've only recently added some property-based testing to this repository for some happy-path behavior.
I'd love to make this test suite more rigorous.
The most significant hole right now is all the log in `src/lib.rs` which handles variables nuances of a feed body.
The test suite only contains a single valid RSS 2.0 feed.
It would be great to generate test strategies that provide far more coverage of both RSS and Atom feeds.

Another thing that is interesting is the potential holes revealed by `cargo-mutant`.
We've added a GitHub workflow for it to show the holes.

Finally, `proptest` tests for `src/feedfetcher.rs` are excessively slow.
We should be able to speed those up.
