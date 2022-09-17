# openring-rs

This is a rust-port of Drew DeVault's [openring](https://git.sr.ht/~sircmpwn/openring), with the
primary differences being:
- the template is provided as a argument, not read from stdin
- we show a little progress bar

`openring-rs` is a tool for generating a webring from RSS feeds, so you can populate a template with
articles from those feeds and embed them in your own blog. An example template is provided in
`in.html`.

## Usage

```
openring 0.1.0

USAGE:
    openring [OPTIONS] --template-file <FILE>

OPTIONS:
    -h, --help                           Print help information
    -n, --num-articles <NUM_ARTICLES>    Total number of articles to fetch [default: 3]
    -p, --per-source <PER_SOURCE>        Number of most recent articles to get from each feed [default: 1]
    -s, --urls <URLS>                    A specific URL to consider (can be repeated)
    -S, --url-file <FILE>                File with URLs of RSS feeds to read
    -t, --template-file <FILE>           Tera template file
    -V, --version                        Print version information
```

## Using Tera Templates

The templates supported by `openring-rs` are written using [Tera](https://tera.netlify.app/). Please
refer to the Tera documentation for details.

## Why a Rust Port?

Just for fun. You probably want to use Drew's stuff, it's likely better. But, this works for me.
