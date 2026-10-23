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

`openring-rs` is a tool for generating a webring from Atom/RSS feeds, so you can populate a template with articles from those feeds and embed them in your own blog.
An example template is provided in `in.html`.

This is a Rust-port of [openring](https://git.sr.ht/~sircmpwn/openring), with the primary improvements being:
- we respect throttling and send conditional requests by default via caching (disable with `--no-cache`)
- the template is written using [Tera](https://keats.github.io/tera/) and is provided as an argument, not read from stdin
- we show a little progress bar
- we fetch all feeds concurrently
- we provide better error messages (via [miette](https://github.com/zkat/miette))
- we allow filtering feeds with `--before`
- we support per-feed weighting, so prolific feeds don't dominate the ring
- we generate a summary from the source if one is missing in the feed

## Demo
To see this in action, you can look at the footer of this blog post.

<https://luke.hsiao.dev/blog/openring-rs/>

## Install
### Cargo
```
cargo install --locked openring
```

Or, if you use [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall):

```
cargo binstall openring
```

### Arch
On Arch Linux, install from the [AUR](https://aur.archlinux.org/) using your preferred helper (e.g. [`paru`](https://github.com/Morganamilo/paru) or [`yay`](https://github.com/Jguer/yay)):

```
paru -S openring-rs       # builds from source
paru -S openring-rs-bin   # prebuilt binary
```

Both packages provide the `openring` binary and conflict with the original Go-based [`openring`](https://aur.archlinux.org/packages/openring) AUR package, so only one may be installed at a time.

### mise
[mise](https://mise.jdx.dev/) can install a prebuilt binary directly from the GitHub release, verifying its checksum and SLSA provenance:

```
mise use -g github:lukehsiao/openring-rs
```

This puts the `openring` binary on your `PATH`.

## Usage
```
A webring for static site generators written in Rust

Usage: openring [OPTIONS] --template-file <FILE>

Options:
  -n, --num-articles <NUM_ARTICLES>    Total number of articles to fetch [default: 3]
  -p, --per-source <PER_SOURCE>        Number of most recent articles to get from each feed
                                       [default: 1]
  -S, --url-file <FILE>                File with URLs of Atom/RSS feeds to read (one URL per line,
                                       optionally followed by an integer weight; see --help)
  -t, --template-file <FILE>           Tera template file
  -s, --url <URL>                      A single URL to consider, optionally followed by a weight,
                                       e.g. `https://example.com/feed.xml 7` (can be repeated to
                                       specify multiple)
  -b, --before <BEFORE>                Only include articles before this date (in YYYY-MM-DD format)
      --no-cache                       Do NOT use request cache stored on disk
      --max-cache-age <MAX_CACHE_AGE>  Discard a cached feed once this long has passed since its
                                       server last sent, confirmed, or rate-limited it [default:
                                       30d]
      --seed <U64>                     Seed the random selection used by weighted feeds, for
                                       reproducible output
  -v, --verbose...                     Increase logging verbosity
  -q, --quiet...                       Decrease logging verbosity
  -h, --help                           Print help (see more with '--help')
  -V, --version                        Print version
```

## Feed weighting
A webring sorted purely by recency lets one prolific feed dominate: if a blog in your ring posts daily, its newest article is almost always among the most recent, so it appears in the output on every single build.

To distribute inclusion more evenly, follow any URL (in the urls file or after `-s`) with an integer weight:

```
# urls.txt
https://quiet.example/feed.xml
https://daily.example/feed.xml 7
```

A feed with weight N contributes a random pick from its N newest articles instead of always its newest.
A daily blog with weight 7 offers, on average, a several-day-old article to the recency sort, so it competes like a weekly blog instead of always winning.
Choose N as roughly the number of posts the feed publishes in the time your other feeds publish one.

Details worth knowing:
- Feeds without a weight behave exactly as before; rings without weights produce unchanged output.
- The random rank is drawn from the full pool of N. If it lands past the articles the feed actually has (say rank 9 of weight 10, for a feed with only 3 recent articles), the feed sits out that build, so sparse feeds participate proportionally instead of being over-represented.
- With `--per-source p`, a weighted feed contributes at most `min(p, N)` distinct picks from the same pool of N.
- Listing the same feed twice with different weights is an error.
- Selection re-rolls on every run. Use `--seed` to make it reproducible, e.g. `--seed "$(date +%Y%m%d)"` rotates daily while keeping rebuilds within the same day stable.

## Using Tera templates
The templates supported by `openring-rs` are written using [Tera](https://keats.github.io/tera/) 2.x.
Please refer to the Tera documentation for details.
Templates written for older `openring-rs` releases (Tera 1.x) may need updating; see the [Tera migration guide](https://github.com/Keats/tera/blob/master/MIGRATION.md).
Notably, `linebreaksbr` is now `newlines_to_br`.

On top of Tera's built-ins, `openring-rs` registers the `date`, `striptags`, `urlencode`, and `urlencode_strict` filters and the `now()` function from [tera-contrib](https://crates.io/crates/tera-contrib), since Tera 2.0 moved them out of core.
`date` takes a strftime `format` (default `%Y-%m-%d`) and an IANA `timezone` (default UTC), e.g. `{{ article.timestamp | date(format="%B %d, %Y") }}`.

Feed text arrives already made safe for HTML, so render it with `| safe`, as the bundled `in.html` does.
`article.title` and `article.source_title` are plain text with `&`, `<`, `>`, `"`, and `'` escaped, so they are safe in element content and in quoted attributes.
`article.summary` is HTML reduced to a safe set of tags, but only as a whole and as element content: apply `striptags` before `truncate` or any other filter that cuts text, as `in.html` does, and never put it inside an attribute.
`article.link` and `article.source_link` are `http` or `https` URLs with quotes, `<`, `>`, and spaces escaped, so they are safe in quoted attributes.
Without `| safe`, Tera escapes the text a second time, and an apostrophe shows as `&#39;` and a double quote as `&quot;`.

## Caching
We use OS-standard locations for caching.

- **Linux**: `$XDG_CACHE_HOME/openring/cache.json` or `$HOME/.cache/openring/cache.json`
- **macOS**: `$HOME/Library/Caches/dev.hsiao.openring/cache.json`
- **Windows**: `{FOLDERID_LocalAppData}\hsiao\openring\cache\cache.json`

The cache file is simple JSON.
Feed bodies are stored as base64-encoded bytes so the original transfer encoding survives for the parser; the other fields are plain text.

The cache prevents refetching a feed only while the feed source's 429 rate limit lasts.
In this case, we respect `Retry-After`, or default to 4 hours.
Otherwise, we use the cache to send conditional requests by respecting the `ETag` and `Last-Modified` headers.

When a fetch fails for a reason that may pass, such as a network error, a server error, or a response that is empty or not a feed, `openring-rs` serves the cached copy and says so: a yellow `Stale` line in a terminal, or a warning in the log.
A `404` or `410` means the feed is gone, so it is reported as an error and never covered up by the cache.
A cached copy is dropped once `--max-cache-age` (30 days by default) has passed since its server last sent, confirmed, or rate-limited it, so a dead feed eventually leaves the output.

## Why a Rust port?
Just for fun.
