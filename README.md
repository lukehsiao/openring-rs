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
    <img src="https://img.shields.io/github/actions/workflow/status/lukehsiao/openring-rs/general.yml" alt="Build Status"></a>
  <a href="https://crates.io/crates/openring">
    <img src="https://img.shields.io/crates/v/openring" alt="Version">
  </a>
  <img src="https://img.shields.io/crates/l/openring" alt="License">
</div>
<br>

This is a rust-port of Drew DeVault's [openring](https://git.sr.ht/~sircmpwn/openring), with the
primary differences being:
- the template is provided as a argument, not read from stdin
- we show a little progress bar
- we fetch all feeds concurrently
- we allow filtering feeds with `--before`.

`openring-rs` is a tool for generating a webring from RSS feeds, so you can populate a template with
articles from those feeds and embed them in your own blog. An example template is provided in
`in.html`.

## Install

```
cargo install openring
```

## Usage

```
A webring for static site generators written in Rust

Usage: openring [OPTIONS] --template-file <FILE>

Options:
  -n, --num-articles <NUM_ARTICLES>  Total number of articles to fetch [default: 3]
  -p, --per-source <PER_SOURCE>      Number of most recent articles to get from each feed [default: 1]
  -S, --url-file <FILE>              File with URLs of RSS feeds to read (one URL per line)
  -t, --template-file <FILE>         Tera template file
  -s, --urls <URLS>                  A specific URL to consider (can be repeated)
  -b, --before <BEFORE>              Only include articles before this date (in YYYY-MM-DD format)
  -v, --verbose...                   More output per occurrence
  -q, --quiet...                     Less output per occurrence
  -h, --help                         Print help information (use `--help` for more detail)
  -V, --version                      Print version information
```

## Using Tera Templates

The templates supported by `openring-rs` are written using [Tera](https://tera.netlify.app/). Please
refer to the Tera documentation for details.

## Why a Rust Port?

Just for fun. You probably want to use Drew's stuff, it's likely better. But, this works for me.
