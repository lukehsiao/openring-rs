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

This is a rust-port of Drew DeVault's [openring](https://git.sr.ht/~sircmpwn/openring), with the primary differences being:
- we respect throttling and send conditional requests when using `--cache` (recommended!)
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
  -c, --cache                          Use request cache stored on disk at `.openringcache`
      --max-cache-age <MAX_CACHE_AGE>  Discard all cached requests older than this duration [default: 14d]
  -v, --verbose...                     Increase logging verbosity
  -q, --quiet...                       Decrease logging verbosity
  -h, --help                           Print help (see more with '--help')
  -V, --version                        Print version
```

## Using Tera Templates

The templates supported by `openring-rs` are written using [Tera](https://keats.github.io/tera/).
Please refer to the Tera documentation for details.

## Why a Rust Port?

Just for fun.
