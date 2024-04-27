<h1 align="center">
    ⛓<br>
    openring-rs
</h1>
<div align="center">
    <strong>A tool for generating a webring from RSS feeds.</strong>
</div>
<br>
<div align="center">
  <a href="https://github.com/lukehsiao/openring-rs/actions/workflows/general.yml">
    <img src="https://img.shields.io/github/actions/workflow/status/lukehsiao/openring-rs/general.yml" alt="Build Status">
  </a>
  <a href="https://crates.io/crates/openring">
    <img src="https://img.shields.io/crates/v/openring" alt="Version">
  </a>
  <a href="https://github.com/lukehsiao/openring-rs/blob/main/LICENSE">
    <img src="https://img.shields.io/crates/l/openring" alt="License">
  </a>
</div>
<br>

`openring-rs` is a tool for generating a webring from RSS feeds, so you can populate a template with articles from those feeds and embed them in your own blog. An example template is provided in `in.html`.

This is a rust-port of Drew DeVault's [openring](https://git.sr.ht/~sircmpwn/openring), with the primary differences being:
- the template is provided as an argument, not read from stdin
- we show a little progress bar
- we fetch all feeds concurrently
- we allow filtering feeds with `--before`
- we provide better error messages (via [miette](https://github.com/zkat/miette))

## Install

```
cargo install --locked openring
```

## Usage

```
A webring for static site generators written in Rust

Usage: openring [OPTIONS] --template-file <FILE>

Options:
  -n, --num-articles <NUM_ARTICLES>  Total number of articles to fetch [default: 3]
  -p, --per-source <PER_SOURCE>      Number of most recent articles to get from each feed [default: 1]
  -S, --url-file <FILE>              File with URLs of RSS feeds to read (one URL per line, lines starting with '#' or "//" ignored)
  -t, --template-file <FILE>         Tera template file
  -s, --url <URL>                    A single URL to consider (can be repeated to specify multiple)
  -b, --before <BEFORE>              Only include articles before this date (in YYYY-MM-DD format)
  -v, --verbose...                   More output per occurrence
  -q, --quiet...                     Less output per occurrence
  -h, --help                         Print help (see more with '--help')
  -V, --version                      Print version
```

## Using Tera Templates

The templates supported by `openring-rs` are written using [Tera](https://tera.netlify.app/).
Please refer to the Tera documentation for details.

## Why a Rust Port?

Just for fun.
