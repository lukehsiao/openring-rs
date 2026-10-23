pub mod args;
mod cache;
mod clock;
pub mod error;
mod feedfetcher;
mod fs;
mod http;
mod nullable;
mod summarize;
pub mod terminal;
#[cfg(test)]
mod test_logs;

use std::{
    collections::{HashMap, HashSet},
    io::{self, Write},
    num::NonZeroUsize,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use feed_rs::model::{Entry, Feed, Link};
use jiff::{Timestamp, civil::Date, tz::TimeZone};
use miette::NamedSource;
use rand::{Rng, SeedableRng, rngs::StdRng};
use serde::Serialize;
use tera::Tera;
use tokio::{sync::Semaphore, task::JoinSet};
use tracing::{debug, info, warn};
use url::Url;

use crate::{
    args::Args,
    cache::Cache,
    clock::Clock,
    error::{FeedUrlError, FeedWeightError, OpenringError, Result},
    feedfetcher::fetch_feed,
    fs::Fs,
    http::Http,
    terminal::{Bar, Terminal, Tone},
};

#[derive(Serialize, Debug)]
pub(crate) struct Article {
    link: Url,
    title: String,
    summary: String,
    source_link: Url,
    source_title: String,
    timestamp: Timestamp,
}

/// Whitespace-separated tokens of `line`, each with the byte offset it
/// starts at, so diagnostics can point at the exact offending token.
fn tokens_with_offsets(line: &str) -> Vec<(usize, &str)> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (i, c) in line.char_indices() {
        match (start, c.is_whitespace()) {
            (None, false) => start = Some(i),
            (Some(s), true) => {
                tokens.push((s, &line[s..i]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        tokens.push((s, &line[s..]));
    }
    tokens
}

/// Why a feed line failed to parse, with the byte range of the offending
/// part so file diagnostics can label it precisely.
struct LineIssue {
    span: Range<usize>,
    kind: LineIssueKind,
    help: String,
}

enum LineIssueKind {
    Url,
    Weight,
}

/// A parsed weight together with the byte range of its token, which the file
/// parser needs to label duplicate-weight conflicts discovered only after
/// the line itself parsed.
type SpannedWeight = (NonZeroUsize, Range<usize>);

/// Parse one `URL [WEIGHT]` feed line.
///
/// Tokenization happens before URL parsing on purpose: `Url::parse`
/// percent-encodes interior spaces, so handing it the whole line would
/// silently swallow the weight as part of the URL path.
fn parse_feed_line(line: &str) -> std::result::Result<(Url, Option<SpannedWeight>), LineIssue> {
    let tokens = tokens_with_offsets(line);
    let trimmed_end = tokens.last().map_or(0, |&(off, tok)| off + tok.len());
    let Some(&(first_start, url_token)) = tokens.first() else {
        return Err(LineIssue {
            span: 0..line.len(),
            kind: LineIssueKind::Url,
            help: "expected a feed URL".to_string(),
        });
    };

    let url = Url::parse(url_token).map_err(|e| LineIssue {
        // The whole line is suspect when its first token is not a URL.
        span: first_start..trimmed_end,
        kind: LineIssueKind::Url,
        help: e.to_string(),
    })?;

    let weight = match tokens.len() {
        1 => None,
        2 => {
            let (w_start, w_token) = tokens[1];
            let span = w_start..w_start + w_token.len();
            let weight = w_token.parse::<NonZeroUsize>().map_err(|e| LineIssue {
                span: span.clone(),
                kind: LineIssueKind::Weight,
                help: format!(
                    "the weight after a URL must be a positive integer ({e}); omit it for the default unweighted behavior"
                ),
            })?;
            Some((weight, span))
        }
        _ => {
            let (extra_start, _) = tokens[1];
            return Err(LineIssue {
                span: extra_start..trimmed_end,
                kind: LineIssueKind::Weight,
                help: "expected `URL [WEIGHT]`: at most one integer weight may follow the URL"
                    .to_string(),
            });
        }
    };

    Ok((url, weight))
}

/// Wrap a [`LineIssue`] into the matching diagnostic, with `src` as the
/// source text and the issue's span shifted by `base` into that text.
fn diagnostic_for(issue: LineIssue, src: NamedSource<String>, base: usize) -> OpenringError {
    let span = (base + issue.span.start..base + issue.span.end).into();
    match issue.kind {
        LineIssueKind::Url => FeedUrlError {
            src,
            span,
            help: issue.help,
        }
        .into(),
        LineIssueKind::Weight => FeedWeightError {
            src,
            span,
            help: issue.help,
        }
        .into(),
    }
}

/// Parse one `-s/--url` argument in the same `URL [WEIGHT]` grammar as
/// urls-file lines, with the argument text as the diagnostic source so
/// errors point at the offending token.
fn parse_cli_url(raw: &str) -> Result<(Url, Option<NonZeroUsize>)> {
    parse_feed_line(raw)
        .map(|(url, weight)| (url, weight.map(|(w, _)| w)))
        .map_err(|issue| diagnostic_for(issue, NamedSource::new("-s/--url", raw.to_owned()), 0))
}

/// Merge a newly seen weight for a feed into the weight already on record.
///
/// Absence means "no opinion", so an explicit weight wins over none and
/// repeating the same weight is fine. Two different explicit weights are a
/// contradiction only the user can resolve; the pair comes back as the error.
fn merge_weight(
    existing: &mut Option<NonZeroUsize>,
    incoming: Option<NonZeroUsize>,
) -> std::result::Result<(), (NonZeroUsize, NonZeroUsize)> {
    match (*existing, incoming) {
        (Some(a), Some(b)) if a != b => Err((a, b)),
        (None, Some(b)) => {
            *existing = Some(b);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Record one configured feed, merging duplicates per [`merge_weight`].
fn record_feed(
    feeds: &mut HashMap<Url, Option<NonZeroUsize>>,
    url: Url,
    weight: Option<NonZeroUsize>,
) -> Result<()> {
    if let Some(existing) = feeds.get_mut(&url) {
        merge_weight(existing, weight).map_err(|(a, b)| OpenringError::ConflictingWeightError {
            url: String::from(url),
            a: a.get(),
            b: b.get(),
        })?;
    } else {
        feeds.insert(url, weight);
    }
    Ok(())
}

/// Every configured feed, with `-s` values and the urls file merged into one
/// weight per URL. This is the typed boundary between raw command-line input
/// and the rest of the program.
#[derive(Debug)]
struct FeedSet {
    /// Every feed to fetch, weighted or not.
    urls: Vec<Url>,
    /// Only the explicitly weighted feeds appear here.
    weights: HashMap<Url, NonZeroUsize>,
}

impl FeedSet {
    /// Parse and merge the configured feeds from `-s` values and the urls
    /// file, given as its path and text, rejecting contradictory weights for
    /// the same URL.
    ///
    /// # Errors
    ///
    /// Returns an error when no feeds are configured at all, a `-s` value or
    /// file line fails to parse, or one feed is given two different weights.
    fn resolve(cli_urls: &[String], url_file: Option<(&Path, &str)>) -> Result<Self> {
        let mut configured: HashMap<Url, Option<NonZeroUsize>> = HashMap::new();
        for raw in cli_urls {
            let (url, weight) = parse_cli_url(raw)?;
            record_feed(&mut configured, url, weight)?;
        }
        if let Some((path, text)) = url_file {
            for (url, weight) in parse_urls_from_file(path, text)? {
                record_feed(&mut configured, url, weight)?;
            }
        }
        if configured.is_empty() {
            return Err(OpenringError::FeedMissing);
        }

        let urls = configured.keys().cloned().collect();
        let weights = configured
            .into_iter()
            .filter_map(|(url, weight)| weight.map(|w| (url, w)))
            .collect();
        Ok(FeedSet { urls, weights })
    }
}

/// Parse the urls file, read from `path`, into feed URLs, each with its
/// optional weight.
///
/// Each line is `URL [WEIGHT]`. Blank lines and lines starting with `#` or
/// `//` are ignored, and duplicate URLs merge per [`merge_weight`]. The first
/// invalid line fails the parse with a diagnostic spanning the offending
/// tokens.
fn parse_urls_from_file(path: &Path, file_src: &str) -> Result<HashMap<Url, Option<NonZeroUsize>>> {
    let mut feeds: HashMap<Url, Option<NonZeroUsize>> = HashMap::new();
    let mut offset = 0;
    for raw_line in file_src.split_inclusive('\n') {
        let line = raw_line.trim();
        if !(line.is_empty() || line.starts_with('#') || line.starts_with("//")) {
            let (url, weight) = parse_feed_line(raw_line).map_err(|issue| {
                diagnostic_for(
                    issue,
                    NamedSource::new(path.to_string_lossy(), file_src.to_string()),
                    offset,
                )
            })?;
            match weight {
                Some((w, w_span)) => {
                    if let Err((existing, _)) =
                        merge_weight(feeds.entry(url).or_insert(None), Some(w))
                    {
                        return Err(FeedWeightError {
                            src: NamedSource::new(path.to_string_lossy(), file_src.to_string()),
                            span: (offset + w_span.start..offset + w_span.end).into(),
                            help: format!(
                                "this feed is already listed with weight {existing}; each feed takes a single weight"
                            ),
                        }
                        .into());
                    }
                }
                // The URL is recorded either way; a bare line has no opinion
                // about an existing weight.
                None => {
                    feeds.entry(url).or_insert(None);
                }
            }
        }
        offset += raw_line.len();
    }
    Ok(feeds)
}

/// Cap on in-flight fetches so a long urls file cannot exhaust the process's
/// file descriptors (macOS defaults to 256 per process). 32 keeps the network
/// saturated while staying well below that floor.
const MAX_CONCURRENT_FETCHES: usize = 32;

/// Set the progress bar's message to the list of URLs still in flight.
fn show_pending(bar: &Bar, pending: &HashSet<&Url>) {
    bar.set_message(
        pending
            .iter()
            .map(|u| u.as_str())
            .collect::<Vec<&str>>()
            .join(", "),
    );
}

// Get all feeds from URLs concurrently, sharing one `http` client, as of
// `now`.
//
// Skips feeds if there are errors, and shows progress as fetches finish.
async fn get_feeds_from_urls(
    http: &Http,
    terminal: &Terminal,
    urls: &[Url],
    cache: &Cache,
    now: Timestamp,
) -> Vec<(Feed, Url)> {
    let bar = terminal.bar("Fetching", urls.len());

    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES));

    let mut join_set = JoinSet::new();
    let mut pending_urls: HashSet<&Url> = HashSet::from_iter(urls);

    show_pending(&bar, &pending_urls);

    for url in urls {
        // Cache is a cheap handle to the shared entries.
        let cache_clone = cache.clone();
        // Http is a cheap handle to the shared pool.
        let http = http.clone();
        let semaphore_clone = Arc::clone(&semaphore);
        let url_clone = url.clone();
        join_set.spawn(async move {
            // acquire_owned errors only when the semaphore is closed, and
            // this one never is.
            let _permit = semaphore_clone
                .acquire_owned()
                .await
                .expect("semaphore is never closed");
            let fetch_result = fetch_feed(&url_clone, &http, &cache_clone, now).await;
            (url_clone, fetch_result)
        });
    }
    let mut feeds = Vec::new();

    while let Some(result) = join_set.join_next().await {
        bar.inc();
        match result {
            Ok((url, Ok(fetched))) => {
                pending_urls.remove(&url);
                show_pending(&bar, &pending_urls);
                match &fetched.stale_because {
                    None => bar.status("Fetched", Tone::Good, &url),
                    // The bar is hidden when stderr is not a terminal, and
                    // its lines vanish with it. Only then does the problem
                    // go to the log, so a terminal shows one styled line and
                    // a cron job still gets a warning.
                    Some(e) if terminal.is_visible() => {
                        bar.status("Stale", Tone::Warn, format!("{url} ({e})"));
                    }
                    Some(e) => {
                        warn!(source = url.as_str(), error = %e, "serving cached feed: fetch failed");
                    }
                }
                feeds.push((fetched.document, url));
            }
            Ok((url, Err(e))) => {
                pending_urls.remove(&url);
                show_pending(&bar, &pending_urls);
                // Hidden bars lose their lines, as above.
                if terminal.is_visible() {
                    bar.status("Error", Tone::Bad, format!("{url} ({e})"));
                } else {
                    warn!(source = url.as_str(), error = %e, "skipping feed: fetch failed");
                }
            }
            Err(e) => {
                // A fetch task that panicked or was aborted. The URL is lost
                // with the task, so it cannot be removed from the pending
                // list, but the failure must not be silent.
                warn!(error=%e, "feed fetch task failed");
                bar.status("Error", Tone::Bad, format!("fetch task failed ({e})"));
            }
        }
    }

    bar.finish_and_clear();
    feeds
}

/// Derive summaries for the chosen `articles` whose feeds provided none, by
/// fetching each article's own page (see [`summarize::fetch_summary`]).
///
/// Runs after selection so only the articles that will render trigger a page
/// fetch, and mutates `articles` in place. An article whose page yields
/// nothing keeps its empty summary, exactly as if the feed had one that was
/// blank, so nothing here can fail the run.
async fn fill_missing_summaries(http: &Http, terminal: &Terminal, articles: &mut [Article]) {
    // Owned links so the pending set below borrows from `missing`, leaving
    // `articles` free to take the derived summaries by index afterward.
    let missing: Vec<(usize, Url)> = articles
        .iter()
        .enumerate()
        .filter(|(_, a)| a.summary.is_empty())
        .map(|(i, a)| (i, a.link.clone()))
        .collect();
    if missing.is_empty() {
        return;
    }

    let bar = terminal.bar("Summarizing", missing.len());

    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES));
    let mut pending_urls: HashSet<&Url> = missing.iter().map(|(_, url)| url).collect();
    show_pending(&bar, &pending_urls);

    let mut join_set = JoinSet::new();
    for (idx, url) in &missing {
        let http = http.clone();
        let semaphore_clone = Arc::clone(&semaphore);
        let idx = *idx;
        let url = url.clone();
        join_set.spawn(async move {
            let _permit = semaphore_clone
                .acquire_owned()
                .await
                .expect("semaphore is never closed");
            let summary = summarize::fetch_summary(&http, &url).await;
            (idx, url, summary)
        });
    }

    while let Some(result) = join_set.join_next().await {
        bar.inc();
        match result {
            Ok((idx, url, summary)) => {
                pending_urls.remove(&url);
                show_pending(&bar, &pending_urls);
                if let Some(summary) = summary {
                    bar.status("Summary", Tone::Good, &url);
                    articles[idx].summary = summary;
                }
            }
            Err(e) => {
                // A task that panicked or was aborted; its article keeps the
                // empty summary, but the failure must not be silent.
                warn!(error = %e, "summary fetch task failed");
            }
        }
    }

    bar.finish_and_clear();
}

/// The edges a run talks to the outside world through. Tests build them
/// nulled, so a whole run executes without network, disk, or clock.
struct Deps {
    http: Http,
    fs: Fs,
    clock: Clock,
    /// Where the cache lives, if anywhere.
    cache_path: Option<PathBuf>,
}

/// Fetch every configured feed, render the most recent articles through the
/// template, and write the result to `out`, followed by a newline. Progress
/// shows on `terminal`. `main` passes stdout; tests pass a buffer so they can
/// assert the rendered bytes.
///
/// # Errors
///
/// Returns an error if no feed URLs are given, a `-s/--url` value or url-file
/// line holds an invalid URL or weight, one feed is listed with two different
/// weights, the url file cannot be read, the template file cannot be read or
/// parsed, the template fails to render, or the HTTP client cannot start.
pub async fn run(args: Args, terminal: &Terminal, out: impl Write) -> Result<()> {
    let deps = Deps {
        // One client for the whole run, so every fetch shares a connection
        // pool instead of paying for TLS setup per request, feeds and
        // summary pages alike.
        http: Http::create()?,
        fs: Fs::create(),
        clock: Clock::create(),
        cache_path: Cache::default_path(),
    };
    run_with(&deps, terminal, args, out).await
}

async fn run_with(deps: &Deps, terminal: &Terminal, args: Args, out: impl Write) -> Result<()> {
    debug!(?args);

    // Read and parse the template before anything else: a wrong path or a
    // syntax error should fail in milliseconds, not after fetching every feed.
    let tera = template_engine(&deps.fs.read_to_string(&args.template_file)?)?;

    // One instant for the whole run, so cache pruning and every fetch agree
    // on which entries are too old and which retry windows are open.
    let now = deps.clock.now();
    let cache_path = deps.cache_path.as_deref().filter(|_| !args.no_cache);
    let cache = cache_path.map_or_else(Cache::default, |path| {
        Cache::load(&deps.fs, path, args.max_cache_age, now)
    });

    // Merge -s urls and the urls file into one weight-per-feed view, so
    // duplicate listings collapse and contradictory weights fail fast.
    let url_file = match &args.url_file {
        Some(path) => Some((path.as_path(), deps.fs.read_to_string(path)?)),
        None => None,
    };
    let feed_set = FeedSet::resolve(
        &args.url,
        url_file.as_ref().map(|(path, text)| (*path, text.as_str())),
    )?;

    let feeds = get_feeds_from_urls(&deps.http, terminal, &feed_set.urls, &cache, now).await;

    if let Some(path) = cache_path {
        cache.save(&deps.fs, path);
    }

    // Entropy by default: rotating the weighted picks between runs is the
    // point. A fixed seed reproduces the same picks for tests and stable
    // site builds.
    let mut rng = match args.seed {
        Some(seed) => StdRng::seed_from_u64(seed),
        None => StdRng::from_rng(&mut rand::rng()),
    };
    let mut articles = select_articles(
        feeds,
        args.per_source,
        args.num_articles,
        args.before,
        &feed_set.weights,
        &mut rng,
    )?;

    // Feeds that ship no summary get one derived from the article page
    // itself. Deferred until here so only the articles that will render
    // trigger a page fetch.
    fill_missing_summaries(&deps.http, terminal, &mut articles).await;

    let mut context = tera::Context::new();
    context.insert("articles", &articles);
    let output = tera.render(TEMPLATE_NAME, &context)?;
    write_output(out, &output)
}

/// The name's .html suffix is what turns on Tera's autoescaping.
const TEMPLATE_NAME: &str = "template.html";

/// Build the Tera instance that renders `template`.
///
/// # Errors
///
/// Returns an error if the template has a syntax error or references a
/// filter that does not exist.
fn template_engine(template: &str) -> Result<Tera> {
    let mut tera = Tera::default();
    // Tera 2.0 moved several 1.x built-ins out of core into tera-contrib.
    // Register the ones webring templates use: `date` and `striptags` (the
    // bundled template needs both to format timestamps and flatten summary
    // HTML), plus `urlencode`/`urlencode_strict` and `now()` for share
    // links and "generated on" footers. This must happen before the
    // template is parsed, since tera rejects templates referencing unknown
    // filters at parse time.
    tera.register_filter("date", tera_contrib::dates::date);
    tera.register_filter("striptags", tera_contrib::regex::striptags);
    tera.register_filter("urlencode", tera_contrib::urlencode::urlencode);
    tera.register_filter(
        "urlencode_strict",
        tera_contrib::urlencode::urlencode_strict,
    );
    tera.register_function("now", tera_contrib::dates::now);
    tera.add_raw_template(TEMPLATE_NAME, template)?;
    Ok(tera)
}

/// Write the rendered output to `w`, followed by a newline.
///
/// A closed pipe (e.g. `openring ... | head`) is treated as success: the
/// reader has everything it wants, and Unix convention is to exit quietly
/// rather than panic the way `println!` does on EPIPE.
fn write_output(mut w: impl Write, output: &str) -> Result<()> {
    match writeln!(w, "{output}").and_then(|()| w.flush()) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => Ok(result?),
    }
}

/// Build the sorted, truncated list of articles to render from the fetched feeds.
///
/// Each feed contributes its `per_source` most recent qualifying entries,
/// judged by publication date rather than the order the feed lists them in.
/// A feed with weight N in `weights` instead fills min(`per_source`, N)
/// slots drawn at random from its N most recent qualifying entries, so a
/// prolific feed stops monopolizing the output; slots drawn past the end of
/// what the feed actually has contribute nothing (see
/// [`draw_weighted_slots`]). `before` drops anything published after that
/// date (interpreted in the system timezone) before the caps apply, and
/// `num_articles` caps the final newest-first list.
///
/// Output is a function of the arguments alone: feeds are processed in URL
/// order, so a fixed `rng` reproduces the same picks no matter what order
/// the fetches finished in.
fn select_articles(
    mut feeds: Vec<(Feed, Url)>,
    per_source: usize,
    num_articles: usize,
    before: Option<Date>,
    weights: &HashMap<Url, NonZeroUsize>,
    rng: &mut impl Rng,
) -> Result<Vec<Article>> {
    // Convert the cutoff to an instant once: midnight at the start of
    // `before` in the system timezone.
    let cutoff = before
        .map(|date| date.to_zoned(TimeZone::system()).map(|z| z.timestamp()))
        .transpose()?;

    // Fetches finish in nondeterministic order, and every weighted feed
    // consumes RNG draws, so a fixed iteration order is what makes a fixed
    // seed actually reproduce the output.
    feeds.sort_unstable_by(|(_, a), (_, b)| a.cmp(b));

    let mut articles = Vec::new();
    for (feed, url) in feeds {
        let source_title = resolve_source_title(&feed, &url);
        let source_link = resolve_source_link(&feed, &url)?;
        let mut from_feed = Vec::new();
        let mut incomplete = 0_usize;
        for entry in &feed.entries {
            match build_article(entry, &url, &source_title, &source_link)? {
                Some(article) => {
                    if cutoff.is_none_or(|c| article.timestamp < c) {
                        from_feed.push(article);
                    }
                }
                None => incomplete += 1,
            }
        }
        // One actionable signal instead of a warning per malformed entry: a
        // feed that yields nothing usable would otherwise vanish silently
        // from the output. Entries the user filtered out via --before do not
        // count against the feed.
        if from_feed.is_empty() && incomplete > 0 {
            warn!(
                source = url.as_str(),
                skipped = incomplete,
                "feed contributed no articles: its entries are missing a link, title, or date"
            );
        }
        from_feed.sort_unstable_by(article_order);
        match weights.get(&url) {
            None => from_feed.truncate(per_source),
            Some(&weight) => {
                let eligible = from_feed.len();
                apply_weight(&mut from_feed, weight, per_source, rng);
                if eligible > 0 && from_feed.is_empty() {
                    debug!(
                        source = url.as_str(),
                        weight = weight.get(),
                        eligible,
                        "weighted feed drew only empty slots and sits out this run"
                    );
                }
            }
        }
        articles.append(&mut from_feed);
    }

    articles.sort_unstable_by(article_order);
    articles.truncate(num_articles);
    Ok(articles)
}

/// The 0-based ranks a feed with this weight fills on one run:
/// min(`per_source`, `weight`) distinct ranks drawn uniformly from
/// 0..weight, where rank i means "the feed's i-th newest eligible article".
///
/// Ranks come from the full 0..weight range on purpose. A rank the feed has
/// no article for is an empty slot, so a feed with E < N eligible articles
/// participates in roughly E/N of runs instead of concentrating its odds on
/// the few articles it has.
fn draw_weighted_slots(weight: NonZeroUsize, per_source: usize, rng: &mut impl Rng) -> Vec<usize> {
    rand::seq::index::sample(rng, weight.get(), per_source.min(weight.get())).into_vec()
}

/// Reduce a weighted feed's newest-first eligible articles to the drawn slots.
fn apply_weight(
    from_feed: &mut Vec<Article>,
    weight: NonZeroUsize,
    per_source: usize,
    rng: &mut impl Rng,
) {
    let slots = draw_weighted_slots(weight, per_source, rng);
    let mut rank = 0;
    from_feed.retain(|_| {
        let keep = slots.contains(&rank);
        rank += 1;
        keep
    });
}

/// Newest first, with ties broken by link so the output is identical no
/// matter what order the feeds finished fetching in.
fn article_order(a: &Article, b: &Article) -> std::cmp::Ordering {
    b.timestamp
        .cmp(&a.timestamp)
        .then_with(|| a.link.cmp(&b.link))
}

/// The href of the first link explicitly tagged `rel="alternate"`, if any.
fn find_alternate_link(links: &[Link]) -> Option<&str> {
    links
        .iter()
        .find(|l| l.rel.as_deref() == Some("alternate"))
        .map(|l| l.href.as_str())
}

/// The display title for a feed: its declared title, or the fetch URL's host
/// (domain or IP address) when the feed omits the title or leaves it blank.
fn resolve_source_title(feed: &Feed, feed_url: &Url) -> String {
    match &feed.title {
        Some(t) if !t.content.is_empty() => t.content.clone(),
        // Hostless URLs (e.g. file:) can't be fetched anyway, so the full URL
        // is an adequate last resort.
        _ => feed_url
            .host_str()
            .unwrap_or_else(|| feed_url.as_str())
            .to_owned(),
    }
}

/// The canonical homepage link for a feed.
///
/// Prefers the title's `src`, then an `alternate` link, then any link that is
/// not the feed's own `self` link, and finally falls back to the feed URL.
fn resolve_source_link(feed: &Feed, feed_url: &Url) -> Result<Url> {
    if let Some(src) = feed.title.as_ref().and_then(|t| t.src.as_ref()) {
        return Ok(feed_url.join(src)?);
    }
    if let Some(href) = find_alternate_link(&feed.links) {
        return Ok(feed_url.join(href)?);
    }
    // Ignore "self" rels, which usually link back to the feed itself.
    if let Some(link) = feed.links.iter().find(|l| l.rel.as_deref() != Some("self")) {
        return Ok(feed_url.join(&link.href)?);
    }
    warn!(
        source = feed_url.as_str(),
        "feed is missing root link: falling back to rss feed url."
    );
    Ok(feed_url.clone())
}

/// The best link for an entry, resolved against the feed URL.
///
/// Prefers an `alternate` link, falling back to the first link present.
/// Comment links are never chosen: they point at discussion of the post, not
/// the post itself. `None` when the entry has no other links or the href
/// cannot be parsed; the caller skips such entries like any other incomplete
/// entry.
fn resolve_entry_link(entry: &Entry, feed_url: &Url) -> Option<Url> {
    let href = find_alternate_link(&entry.links).or_else(|| {
        entry
            .links
            .iter()
            .find(|link| link.target.is_none())
            .map(|link| link.href.as_str())
    })?;
    feed_url.join(href).ok()
}

/// The entry's summary text, preferring an explicit `<summary>` and falling back
/// to the content body. `None` when the entry carries neither.
fn raw_summary(entry: &Entry) -> Option<&str> {
    entry
        .summary
        .as_ref()
        .map(|s| s.content.as_str())
        .or_else(|| entry.content.as_ref().and_then(|c| c.body.as_deref()))
}

/// Decode HTML entities, then strip unsafe markup, returning trimmed HTML
/// that is safe to embed even through the template's `| safe` filter.
///
/// Decoding must come first: cleaning and then decoding would turn harmless
/// entity-encoded text like `&lt;script&gt;` back into live markup.
fn sanitize_html(raw: &str) -> String {
    let decoded = html_escape::decode_html_entities(raw);
    ammonia::clean(&decoded).trim().to_string()
}

/// Reduce feed-supplied text like titles to plain text: entities decoded,
/// every tag stripped, script/style bodies dropped, and the remaining text
/// kept entity-escaped so it is safe to embed in HTML.
fn sanitize_text(raw: &str) -> String {
    let decoded = html_escape::decode_html_entities(raw);
    let mut builder = ammonia::Builder::empty();
    builder.clean_content_tags(["script", "style"].into_iter().collect());
    builder.clean(&decoded).to_string().trim().to_string()
}

/// Build a renderable [`Article`] from a feed entry.
///
/// Returns `Ok(None)` exactly when the entry is incomplete: missing a usable
/// link, title, or date. Errors only when the entry's date is out of
/// representable range. Selection policy (date cutoff, per-source caps)
/// belongs to the caller.
fn build_article(
    entry: &Entry,
    feed_url: &Url,
    source_title: &str,
    source_link: &Url,
) -> Result<Option<Article>> {
    let (Some(link), Some(title), Some(date)) = (
        resolve_entry_link(entry, feed_url),
        entry.title.as_ref().map(|t| &t.content),
        entry.published.or(entry.updated),
    ) else {
        // Routine for third-party feeds and recurring every run, so the
        // per-entry detail stays at debug; select_articles warns once per
        // feed when nothing at all is usable.
        debug!(
            entry_links = ?entry.links,
            entry_title = ?entry.title,
            entry_published = ?entry.published,
            entry_updated = ?entry.updated,
            source = feed_url.as_str(),
            "skipping entry: must have link, title, and a date."
        );
        return Ok(None);
    };

    let timestamp = Timestamp::from_second(date.timestamp())?;

    let summary = raw_summary(entry).map_or_else(
        || {
            info!(?link, ?source_link, "no summary or content provided.");
            String::new()
        },
        sanitize_html,
    );

    Ok(Some(Article {
        link,
        // Titles are feed-controlled and the default template embeds them
        // with `| safe`, so they get the same boundary treatment as summaries.
        title: sanitize_text(title),
        summary,
        source_link: source_link.clone(),
        source_title: sanitize_text(source_title),
        timestamp,
    }))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, num::NonZeroUsize};
    use url::Url;

    use feed_rs::model::{Entry, Feed, Link};
    use hegel::{extras::rand as rand_gs, generators};
    use rand::{SeedableRng, rngs::StdRng};

    use super::{
        Article, FeedSet, build_article, draw_weighted_slots, find_alternate_link, merge_weight,
        parse_cli_url, parse_urls_from_file, raw_summary, record_feed, resolve_entry_link,
        resolve_source_link, resolve_source_title, sanitize_html, select_articles, write_output,
    };
    use crate::test_logs::warnings_logged;

    // A writer that always fails with the given kind, standing in for a stdout
    // that has gone away.
    struct FailingWriter(std::io::ErrorKind);

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(self.0, "stub failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_output_appends_newline() {
        let mut buf = Vec::new();
        write_output(&mut buf, "hello").unwrap();
        assert_eq!(buf, b"hello\n");
    }

    #[test]
    fn write_output_treats_broken_pipe_as_success() {
        assert!(write_output(FailingWriter(std::io::ErrorKind::BrokenPipe), "x").is_ok());
    }

    #[test]
    fn write_output_propagates_other_io_errors() {
        assert!(matches!(
            write_output(FailingWriter(std::io::ErrorKind::PermissionDenied), "x"),
            Err(crate::error::OpenringError::IoError(_))
        ));
    }

    // The urls file as `run` hands it over: its path, for diagnostics, and
    // its text.
    fn parse_urls(text: &str) -> crate::error::Result<HashMap<Url, Option<NonZeroUsize>>> {
        parse_urls_from_file(std::path::Path::new("urls.txt"), text)
    }

    #[test]
    fn parse_urls_ignores_comments_and_blank_lines_and_whitespace() {
        let parsed = parse_urls(
            "https://first.example/\n\
             \x20  https://second.example   \n\
             \x20https://first.example   \n\
             # a hash comment\n\
             // a double‑slash comment\n\
             \n",
        )
        .unwrap();

        let expected = HashMap::from([
            (Url::parse("https://first.example/").unwrap(), None),
            (Url::parse("https://second.example/").unwrap(), None),
        ]);
        assert_eq!(parsed, expected);
    }

    // Round-trip: any mix of valid `URL [WEIGHT]` lines, comments, blanks,
    // and stray whitespace parses back to exactly the map written.
    #[hegel::test]
    fn parse_urls_round_trips_urls_through_a_file(tc: hegel::TestCase) {
        use std::fmt::Write as _;

        let n = tc.draw(generators::integers::<usize>().min_value(0).max_value(20));
        let mut expected: HashMap<Url, Option<NonZeroUsize>> = HashMap::new();
        let mut text = String::new();
        for _ in 0..n {
            let url = Url::parse(&tc.draw(generators::urls())).expect("generated URL is valid");
            // A duplicate URL repeats its recorded weight: conflicting
            // weights are a parse error with its own test.
            let weight = match expected.get(&url) {
                Some(&w) => w,
                None => tc
                    .draw(generators::optional(
                        generators::integers::<usize>().min_value(1),
                    ))
                    .and_then(NonZeroUsize::new),
            };
            match weight {
                Some(w) => writeln!(text, "  {url} {w}  "),
                None => writeln!(text, "  {url}  "),
            }
            .expect("writing to a String cannot fail");
            expected.insert(url, weight);
            if tc.draw(generators::booleans()) {
                text.push_str(&tc.draw(hegel::one_of!(
                    generators::just("# comment\n".to_string()),
                    generators::just("// comment\n".to_string()),
                    generators::just("\n".to_string()),
                    generators::just("   \n".to_string()),
                )));
            }
        }

        assert_eq!(parse_urls(&text).unwrap(), expected);
    }

    #[test]
    fn parse_urls_diagnostic_points_at_the_offending_line() {
        // The same text appears first inside a comment; the span must point
        // at the real line, not the first substring match in the file.
        let err = parse_urls("# not a url\nnot a url\n").unwrap_err();
        let crate::error::OpenringError::FeedUrlError(feed_url_error) = err else {
            panic!("expected FeedUrlError, got {err:?}");
        };
        assert_eq!(feed_url_error.span.offset(), "# not a url\n".len());
        assert_eq!(feed_url_error.span.len(), "not a url".len());
    }

    #[test]
    fn parse_urls_reports_invalid_url() {
        // An unparseable line yields a diagnostic error rather than being
        // silently dropped.
        assert!(matches!(
            parse_urls("https://valid.example/\nnot a url\n"),
            Err(crate::error::OpenringError::FeedUrlError(_))
        ));
    }

    #[test]
    fn parse_urls_parses_optional_weights() {
        let parsed =
            parse_urls("https://weighted.example/feed.xml 7\nhttps://plain.example/feed.xml\n")
                .unwrap();

        let expected = HashMap::from([
            (
                Url::parse("https://weighted.example/feed.xml").unwrap(),
                NonZeroUsize::new(7),
            ),
            (Url::parse("https://plain.example/feed.xml").unwrap(), None),
        ]);
        assert_eq!(parsed, expected);
    }

    #[test]
    fn parse_urls_weight_diagnostic_points_at_the_token() {
        let err = parse_urls("https://valid.example/ abc\n").unwrap_err();
        let crate::error::OpenringError::FeedWeightError(e) = err else {
            panic!("expected FeedWeightError, got {err:?}");
        };
        assert_eq!(e.span.offset(), "https://valid.example/ ".len());
        assert_eq!(e.span.len(), "abc".len());
    }

    #[test]
    fn parse_urls_rejects_a_zero_weight() {
        let err = parse_urls("https://valid.example/ 0\n").unwrap_err();
        let crate::error::OpenringError::FeedWeightError(e) = err else {
            panic!("expected FeedWeightError, got {err:?}");
        };
        assert_eq!(e.span.offset(), "https://valid.example/ ".len());
        assert_eq!(e.span.len(), 1);
    }

    #[test]
    fn parse_urls_rejects_trailing_garbage_after_the_weight() {
        let err = parse_urls("https://valid.example/ 7 9\n").unwrap_err();
        let crate::error::OpenringError::FeedWeightError(e) = err else {
            panic!("expected FeedWeightError, got {err:?}");
        };
        // The span covers everything after the URL.
        assert_eq!(e.span.offset(), "https://valid.example/ ".len());
        assert_eq!(e.span.len(), "7 9".len());
    }

    #[test]
    fn parse_urls_rejects_conflicting_duplicate_weights() {
        let err = parse_urls("https://valid.example/ 3\nhttps://valid.example/ 7\n").unwrap_err();
        let crate::error::OpenringError::FeedWeightError(e) = err else {
            panic!("expected FeedWeightError, got {err:?}");
        };
        // The diagnostic points at the second line's weight token and names
        // the weight it contradicts.
        let second_weight_offset = "https://valid.example/ 3\nhttps://valid.example/ ".len();
        assert_eq!(e.span.offset(), second_weight_offset);
        assert_eq!(e.span.len(), 1);
        assert!(e.help.contains('3'), "help: {}", e.help);
    }

    #[test]
    fn parse_urls_merges_duplicates_per_weight_policy() {
        // Same explicit weight twice, explicit after bare, bare after
        // explicit: all collapse to one entry holding the explicit weight.
        let parsed = parse_urls(
            "https://a.example/ 7\n\
             https://a.example/ 7\n\
             https://b.example/\n\
             https://b.example/ 3\n\
             https://c.example/ 4\n\
             https://c.example/\n",
        )
        .unwrap();

        let expected = HashMap::from([
            (
                Url::parse("https://a.example/").unwrap(),
                NonZeroUsize::new(7),
            ),
            (
                Url::parse("https://b.example/").unwrap(),
                NonZeroUsize::new(3),
            ),
            (
                Url::parse("https://c.example/").unwrap(),
                NonZeroUsize::new(4),
            ),
        ]);
        assert_eq!(parsed, expected);
    }

    #[test]
    fn parse_cli_url_accepts_url_and_optional_weight() {
        let (url, weight) = parse_cli_url("https://example.com/feed.xml 7").unwrap();
        assert_eq!(url.as_str(), "https://example.com/feed.xml");
        assert_eq!(weight, NonZeroUsize::new(7));

        let (_, bare) = parse_cli_url("https://example.com/feed.xml").unwrap();
        assert_eq!(bare, None);
    }

    #[test]
    fn parse_cli_url_rejects_malformed_values() {
        for bad in [
            "",
            "not a url",
            "https://example.com/ 0",
            "https://example.com/ -3",
            "https://example.com/ abc",
            "https://example.com/ 7 9",
        ] {
            assert!(parse_cli_url(bad).is_err(), "accepted: {bad:?}");
        }
    }

    // A bad -s value gets the same span-pointing diagnostic as a urls-file
    // line: the label sits exactly on the offending token.
    #[test]
    fn parse_cli_url_diagnostic_points_at_the_weight_token() {
        let err = parse_cli_url("https://luke.hsiao.dev/atom.xml X").unwrap_err();
        let crate::error::OpenringError::FeedWeightError(e) = err else {
            panic!("expected FeedWeightError, got {err:?}");
        };
        assert_eq!(e.span.offset(), "https://luke.hsiao.dev/atom.xml ".len());
        assert_eq!(e.span.len(), 1);
    }

    // The parser must accept or reject, never panic, whatever the value holds.
    #[hegel::test]
    fn parse_cli_url_never_panics(tc: hegel::TestCase) {
        let line = tc.draw(generators::text());
        let _ = parse_cli_url(&line);
    }

    #[test]
    fn merge_weight_lets_explicit_beat_absent_and_rejects_conflicts() {
        let mut recorded = None;
        merge_weight(&mut recorded, NonZeroUsize::new(3)).unwrap();
        assert_eq!(recorded, NonZeroUsize::new(3));

        // A bare duplicate has no opinion; the explicit weight stays.
        merge_weight(&mut recorded, None).unwrap();
        assert_eq!(recorded, NonZeroUsize::new(3));

        // Repeating the same weight is fine.
        merge_weight(&mut recorded, NonZeroUsize::new(3)).unwrap();
        assert_eq!(recorded, NonZeroUsize::new(3));

        // A different weight is a contradiction carrying both values.
        assert_eq!(
            merge_weight(&mut recorded, NonZeroUsize::new(7)),
            Err((
                NonZeroUsize::new(3).expect("non-zero"),
                NonZeroUsize::new(7).expect("non-zero")
            ))
        );
    }

    #[test]
    fn record_feed_reports_conflicting_weights_for_the_url() {
        let mut feeds = HashMap::new();
        record_feed(&mut feeds, feed_url(), NonZeroUsize::new(3)).unwrap();
        record_feed(&mut feeds, feed_url(), None).unwrap();

        let err = record_feed(&mut feeds, feed_url(), NonZeroUsize::new(7)).unwrap_err();
        assert!(matches!(
            err,
            crate::error::OpenringError::ConflictingWeightError { .. }
        ));
    }

    #[test]
    fn feed_set_resolve_merges_cli_and_file_sources() {
        let file = "https://a.example/ 7\nhttps://b.example/\n";
        let cli = [
            "https://b.example/ 3".to_string(),
            "https://c.example/".to_string(),
        ];

        let feed_set =
            FeedSet::resolve(&cli, Some((std::path::Path::new("urls.txt"), file))).unwrap();

        assert_eq!(feed_set.urls.len(), 3);
        let weight_of = |u: &str| feed_set.weights.get(&Url::parse(u).unwrap()).copied();
        assert_eq!(weight_of("https://a.example/"), NonZeroUsize::new(7));
        // The file's bare listing defers to the CLI's explicit weight.
        assert_eq!(weight_of("https://b.example/"), NonZeroUsize::new(3));
        assert_eq!(weight_of("https://c.example/"), None);
    }

    #[test]
    fn feed_set_resolve_rejects_cross_source_conflicts() {
        let cli = ["https://a.example/ 3".to_string()];

        let err = FeedSet::resolve(
            &cli,
            Some((std::path::Path::new("urls.txt"), "https://a.example/ 7\n")),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            crate::error::OpenringError::ConflictingWeightError { .. }
        ));
    }

    #[test]
    fn feed_set_resolve_requires_at_least_one_feed() {
        assert!(matches!(
            FeedSet::resolve(&[], None),
            Err(crate::error::OpenringError::FeedMissing)
        ));
    }

    const FEED_URL: &str = "https://example.com/feed.xml";

    fn feed_url() -> Url {
        Url::parse(FEED_URL).unwrap()
    }

    // Parse a feed fixture the same way production does.
    fn parse_feed(xml: &str) -> Feed {
        feed_rs::parser::parse(xml.as_bytes()).unwrap()
    }

    // Pair a parsed feed with the URL it was "fetched" from.
    fn feed(url: &str, xml: &str) -> (Feed, Url) {
        (parse_feed(xml), Url::parse(url).unwrap())
    }

    // Wrap entry XML in a minimal Atom feed (title + homepage link present).
    fn atom(entries: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>Example Blog</title>
                <link href="https://example.com/"/>
                <updated>2020-01-01T00:00:00Z</updated>
                {entries}
            </feed>"#
        )
    }

    // The first entry of a single-entry Atom fixture, for entry-level helpers.
    fn first_entry(entry_xml: &str) -> Entry {
        parse_feed(&atom(entry_xml))
            .entries
            .into_iter()
            .next()
            .unwrap()
    }

    fn link(href: &str, rel: Option<&str>) -> Link {
        Link {
            href: href.to_owned(),
            target: None,
            rel: rel.map(str::to_owned),
            media_type: None,
            href_lang: None,
            title: None,
            length: None,
        }
    }

    // Most tests exercise the unweighted path, where the weights map and RNG
    // are inert; this keeps those call sites stable as the signature grows.
    fn select_unweighted(
        feeds: Vec<(Feed, Url)>,
        per_source: usize,
        num_articles: usize,
        before: Option<jiff::civil::Date>,
    ) -> crate::error::Result<Vec<Article>> {
        select_articles(
            feeds,
            per_source,
            num_articles,
            before,
            &HashMap::new(),
            &mut StdRng::seed_from_u64(0),
        )
    }

    // A feed at `url` with `k` complete entries, where entry `rank` (0-based)
    // is the rank-th newest and is titled with its rank.
    fn ranked_feed_at(url: &str, k: usize) -> (Feed, Url) {
        use std::fmt::Write as _;

        let mut entries = String::new();
        for rank in 0..k {
            let day = i64::try_from(k - rank).expect("k is small");
            let published = jiff::Timestamp::from_second(1_600_000_000 + 86_400 * day)
                .expect("timestamp in range");
            write!(
                entries,
                r#"<entry>
                    <title>{rank}</title>
                    <link href="{url}/{rank}"/>
                    <published>{published}</published>
                    <summary>x</summary>
                </entry>"#
            )
            .expect("writing to a String is infallible");
        }
        feed(url, &atom(&entries))
    }

    fn ranked_feed(k: usize) -> (Feed, Url) {
        ranked_feed_at(FEED_URL, k)
    }

    fn titles(articles: &[Article]) -> Vec<String> {
        articles.iter().map(|a| a.title.clone()).collect()
    }

    fn links(articles: &[Article]) -> Vec<String> {
        articles.iter().map(|a| a.link.to_string()).collect()
    }

    #[test]
    fn find_alternate_link_picks_the_alternate_rel() {
        let links = vec![
            link("https://example.com/feed.xml", Some("self")),
            link("https://example.com/home", Some("alternate")),
        ];
        assert_eq!(
            find_alternate_link(&links),
            Some("https://example.com/home")
        );
        assert_eq!(find_alternate_link(&links[..1]), None);
    }

    #[test]
    fn resolve_source_title_prefers_title_then_domain() {
        // A declared, non-empty title is used verbatim.
        assert_eq!(
            resolve_source_title(&parse_feed(&atom("")), &feed_url()),
            "Example Blog"
        );

        // An empty or absent title falls back to the fetch URL's domain. Both an
        // empty <title> and a missing one resolve the same way, so this holds
        // however feed-rs chooses to represent the blank case.
        let blank = parse_feed(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title></title>
                <link href="https://example.com/"/>
                <updated>2020-01-01T00:00:00Z</updated>
            </feed>"#,
        );
        assert_eq!(resolve_source_title(&blank, &feed_url()), "example.com");
    }

    // A titleless feed must get a fallback title for any URL a feed can be
    // fetched from, including IP-address hosts where `Url::domain()` is None.
    #[hegel::test]
    fn resolve_source_title_falls_back_for_any_http_host(tc: hegel::TestCase) {
        use hegel::generators::Generator;
        let host = tc.draw(hegel::one_of!(
            generators::domains(),
            generators::ip_addresses().v4().map(|ip| ip.to_string()),
            // IPv6 hosts go in brackets inside a URL.
            generators::ip_addresses().v6().map(|ip| format!("[{ip}]")),
        ));
        let url = Url::parse(&format!("http://{host}/feed.xml")).expect("generated host is valid");

        let blank = parse_feed(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title></title>
                <link href="https://example.com/"/>
                <updated>2020-01-01T00:00:00Z</updated>
            </feed>"#,
        );
        let title = resolve_source_title(&blank, &url);
        assert_eq!(title, url.host_str().unwrap());
    }

    #[test]
    fn resolve_source_link_prefers_alternate_then_other_then_feed_url() {
        // An explicit alternate link wins over a self link.
        let alt = parse_feed(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>t</title>
                <link rel="self" href="https://example.com/feed.xml"/>
                <link rel="alternate" href="https://example.com/home"/>
                <updated>2020-01-01T00:00:00Z</updated>
            </feed>"#,
        );
        assert_eq!(
            resolve_source_link(&alt, &feed_url()).unwrap().as_str(),
            "https://example.com/home"
        );

        // With no alternate, any link that is not the feed's own `self` is used.
        let other = parse_feed(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>t</title>
                <link rel="self" href="https://example.com/feed.xml"/>
                <link rel="related" href="https://example.com/related"/>
                <updated>2020-01-01T00:00:00Z</updated>
            </feed>"#,
        );
        assert_eq!(
            resolve_source_link(&other, &feed_url()).unwrap().as_str(),
            "https://example.com/related"
        );
    }

    #[test]
    fn resolve_source_link_falls_back_to_feed_url_without_usable_links() {
        // A titleless feed whose only link is `self` has nothing else to point
        // at, so the feed URL itself is used. This also guards the old panic on
        // a missing <title> (it used to `unwrap`).
        let bare = parse_feed(
            r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <link rel="self" href="https://example.com/feed.xml"/>
                <updated>2020-01-01T00:00:00Z</updated>
            </feed>"#,
        );
        assert_eq!(resolve_source_link(&bare, &feed_url()).unwrap(), feed_url());
    }

    #[test]
    fn resolve_entry_link_resolves_relative_and_skips_linkless() {
        // An absolute link is returned unchanged.
        let abs = first_entry(
            r#"<entry>
                <title>t</title>
                <link href="https://other.example/abs"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>"#,
        );
        assert_eq!(
            resolve_entry_link(&abs, &feed_url()).unwrap().as_str(),
            "https://other.example/abs"
        );

        // A relative link is resolved against the feed origin.
        let rel = first_entry(
            r#"<entry>
                <title>t</title>
                <link href="/rel-path"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>"#,
        );
        assert_eq!(
            resolve_entry_link(&rel, &feed_url()).unwrap().as_str(),
            "https://example.com/rel-path"
        );

        // An entry with no links has no resolvable link; the caller skips it.
        let none = first_entry(
            r"<entry>
                <title>t</title>
                <published>2020-01-01T00:00:00Z</published>
            </entry>",
        );
        assert_eq!(resolve_entry_link(&none, &feed_url()), None);
    }

    #[test]
    fn resolve_entry_link_ignores_comment_links() {
        // feed-rs lists RSS `<comments>` and `wfw:commentRss` as entry links,
        // in document order, so a comments link can come first. The article
        // must point at the post itself.
        let feed = parse_feed(
            r#"<?xml version="1.0"?>
            <rss version="2.0" xmlns:wfw="http://wellformedweb.org/CommentAPI/">
                <channel>
                    <title>t</title>
                    <link>https://example.com/</link>
                    <item>
                        <title>a</title>
                        <comments>https://example.com/a#comments</comments>
                        <wfw:commentRss>https://example.com/a/feed</wfw:commentRss>
                        <link>https://example.com/a</link>
                    </item>
                    <item>
                        <title>b</title>
                        <comments>https://example.com/b#comments</comments>
                    </item>
                </channel>
            </rss>"#,
        );
        let links: Vec<_> = feed
            .entries
            .iter()
            .map(|entry| resolve_entry_link(entry, &feed_url()).map(String::from))
            .collect();
        assert_eq!(links, [Some("https://example.com/a".to_owned()), None]);
    }

    #[test]
    fn raw_summary_prefers_summary_then_content_then_none() {
        let summary = first_entry(
            r#"<entry>
                <title>t</title>
                <link href="https://example.com/a"/>
                <published>2020-01-01T00:00:00Z</published>
                <summary>the summary</summary>
            </entry>"#,
        );
        assert_eq!(raw_summary(&summary), Some("the summary"));

        let content_only = first_entry(
            r#"<entry>
                <title>t</title>
                <link href="https://example.com/a"/>
                <published>2020-01-01T00:00:00Z</published>
                <content type="html">&lt;p&gt;body&lt;/p&gt;</content>
            </entry>"#,
        );
        assert!(raw_summary(&content_only).unwrap().contains("body"));

        let neither = first_entry(
            r#"<entry>
                <title>t</title>
                <link href="https://example.com/a"/>
                <published>2020-01-01T00:00:00Z</published>
            </entry>"#,
        );
        assert_eq!(raw_summary(&neither), None);
    }

    #[test]
    fn sanitize_html_strips_scripts_keeps_text_and_trims() {
        let out = sanitize_html("  <p>Safe</p><script>alert(1)</script> Tom &amp; Jerry  ");
        // ammonia removes the <script> and keeps text entity-escaped, so the
        // result stays safe even through the template's `| safe` filter.
        assert!(!out.contains("script"));
        assert!(out.contains("Safe"));
        assert!(out.contains("Tom &amp; Jerry"));
        assert_eq!(out, out.trim());
    }

    // No input, however it encodes its markup, may come out of sanitization
    // with a live <script> tag. Decoding entities after cleaning used to turn
    // harmless text like "&lt;script&gt;" back into real markup, which the
    // default template then embedded raw via `| safe`.
    #[hegel::test]
    fn sanitize_html_never_emits_live_markup(tc: hegel::TestCase) {
        let payload = tc.draw(hegel::one_of!(
            generators::just("<script>alert(1)</script>".to_string()),
            generators::just("&lt;script&gt;alert(1)&lt;/script&gt;".to_string()),
            generators::just("&amp;lt;script&amp;gt;alert(1)&amp;lt;/script&amp;gt;".to_string()),
            generators::text(),
        ));
        let prefix = tc.draw(generators::text().max_size(16));
        let suffix = tc.draw(generators::text().max_size(16));
        let out = sanitize_html(&format!("{prefix}{payload}{suffix}"));
        assert!(
            !out.to_lowercase().contains("<script"),
            "live script tag in sanitized output: {out:?}"
        );
    }

    #[test]
    fn build_article_sanitizes_title_and_source_title() {
        let source_link = Url::parse("https://example.com/").unwrap();
        // The XML parser decodes the entities, so the in-memory title holds
        // real <script> markup, exactly as a hostile feed would deliver it.
        let entry = first_entry(
            r#"<entry>
                <title>&lt;script&gt;alert(1)&lt;/script&gt;Hello</title>
                <link href="https://example.com/x"/>
                <published>2020-01-01T00:00:00Z</published>
                <summary>s</summary>
            </entry>"#,
        );
        let article = build_article(
            &entry,
            &feed_url(),
            "<script>evil()</script>Src",
            &source_link,
        )
        .unwrap()
        .unwrap();
        assert_eq!(article.title, "Hello");
        assert_eq!(article.source_title, "Src");
    }

    #[test]
    fn build_article_assembles_fields_and_skips_incomplete() {
        let source_link = Url::parse("https://example.com/").unwrap();
        let complete = first_entry(
            r#"<entry>
                <title>Complete Post</title>
                <link href="https://example.com/complete"/>
                <published>2024-01-01T00:00:00Z</published>
                <summary>hello</summary>
            </entry>"#,
        );

        // Every field of a complete entry flows through, source info included.
        // The date cutoff is select_articles' concern, exercised there.
        let built = build_article(&complete, &feed_url(), "Src", &source_link)
            .unwrap()
            .unwrap();
        assert_eq!(built.title, "Complete Post");
        assert_eq!(built.link.as_str(), "https://example.com/complete");
        assert_eq!(built.summary, "hello");
        assert_eq!(built.source_title, "Src");
        assert_eq!(built.source_link, source_link);

        // An entry missing its date is skipped rather than erroring.
        let dateless = first_entry(
            r#"<entry>
                <title>Dateless</title>
                <link href="https://example.com/x"/>
                <summary>s</summary>
            </entry>"#,
        );
        assert!(
            build_article(&dateless, &feed_url(), "Src", &source_link)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn select_articles_extracts_basic_fields() {
        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>First Post</title>
                    <link href="https://example.com/first"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>Hello world</summary>
                </entry>"#,
            ),
        )];

        let articles = select_unweighted(feeds, 1, 10, None).unwrap();
        assert_eq!(articles.len(), 1);
        let a = &articles[0];
        assert_eq!(a.link.as_str(), "https://example.com/first");
        assert_eq!(a.title, "First Post");
        assert_eq!(a.summary, "Hello world");
        assert_eq!(a.source_title, "Example Blog");
        assert_eq!(a.source_link.as_str(), "https://example.com/");
    }

    #[test]
    fn select_articles_warns_once_when_a_feed_yields_nothing() {
        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r"<entry>
                    <title>No Link A</title>
                    <published>2021-01-01T00:00:00Z</published>
                </entry>
                <entry>
                    <title>No Link B</title>
                    <published>2021-01-02T00:00:00Z</published>
                </entry>",
            ),
        )];

        let logs = warnings_logged(|| {
            let articles = select_unweighted(feeds, 10, 10, None).unwrap();
            assert!(articles.is_empty());
        });
        // One aggregated warning naming the feed and the count, not one line
        // per malformed entry.
        assert_eq!(
            logs.matches("contributed no articles").count(),
            1,
            "logs: {logs}"
        );
        assert!(logs.contains("skipped=2"), "logs: {logs}");
    }

    #[test]
    fn select_articles_is_quiet_about_partial_skips() {
        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>No Link</title>
                    <published>2021-01-01T00:00:00Z</published>
                </entry>
                <entry>
                    <title>Good</title>
                    <link href="https://example.com/g"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>"#,
            ),
        )];

        let logs = warnings_logged(|| {
            let articles = select_unweighted(feeds, 10, 10, None).unwrap();
            assert_eq!(articles.len(), 1);
        });
        // A feed that still renders something is not warn-worthy; the
        // per-entry detail lives at debug level.
        assert_eq!(logs, "", "expected no warnings for a partially usable feed");
    }

    #[test]
    fn select_articles_is_quiet_when_only_the_cutoff_filters() {
        use jiff::civil::date;

        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>New</title>
                    <link href="https://example.com/new"/>
                    <published>2024-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>"#,
            ),
        )];

        let logs = warnings_logged(|| {
            let articles = select_unweighted(feeds, 10, 10, Some(date(2022, 1, 1))).unwrap();
            assert!(articles.is_empty());
        });
        // --before is the user's own setting; filtering on it is not a fault
        // of the feed and must not warn.
        assert_eq!(logs, "", "expected no warnings for cutoff-only filtering");
    }

    #[test]
    fn select_articles_skips_malformed_entries_instead_of_aborting() {
        // One linkless entry in one feed must not take down the whole run;
        // it is skipped exactly like entries missing a title or date.
        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>No Link</title>
                    <published>2021-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Good</title>
                    <link href="https://example.com/good"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>"#,
            ),
        )];

        let articles = select_unweighted(feeds, 10, 10, None).unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Good");
    }

    #[test]
    fn select_articles_caps_entries_per_source() {
        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>Newest</title>
                    <link href="https://example.com/3"/>
                    <published>2022-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Middle</title>
                    <link href="https://example.com/2"/>
                    <published>2021-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Oldest</title>
                    <link href="https://example.com/1"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>"#,
            ),
        )];

        // per_source = 1 keeps only the most recent entry.
        let articles = select_unweighted(feeds, 1, 10, None).unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Newest");
    }

    #[test]
    fn select_articles_picks_most_recent_per_source_regardless_of_feed_order() {
        // Entries listed oldest-first: per_source must still pick by date,
        // not document order. Nothing in RSS/Atom promises newest-first.
        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>Oldest</title>
                    <link href="https://example.com/1"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Middle</title>
                    <link href="https://example.com/2"/>
                    <published>2021-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Newest</title>
                    <link href="https://example.com/3"/>
                    <published>2022-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>"#,
            ),
        )];

        let articles = select_unweighted(feeds, 1, 10, None).unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Newest");
    }

    #[test]
    fn select_articles_per_source_counts_only_qualifying_entries() {
        use jiff::civil::date;

        // The newest entry falls after --before; it must not use up the
        // per-source budget and shadow the older entry that qualifies.
        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>New</title>
                    <link href="https://example.com/new"/>
                    <published>2024-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Old</title>
                    <link href="https://example.com/old"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>"#,
            ),
        )];

        let articles = select_unweighted(feeds, 1, 10, Some(date(2022, 1, 1))).unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Old");
    }

    #[test]
    fn select_articles_sorts_newest_first_and_caps_total() {
        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>Oldest</title>
                    <link href="https://example.com/1"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Newest</title>
                    <link href="https://example.com/3"/>
                    <published>2022-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>Middle</title>
                    <link href="https://example.com/2"/>
                    <published>2021-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>"#,
            ),
        )];

        // num_articles = 2 keeps the two newest, sorted newest-first.
        let articles = select_unweighted(feeds, 10, 2, None).unwrap();
        assert_eq!(articles.len(), 2);
        assert_eq!(articles[0].title, "Newest");
        assert_eq!(articles[1].title, "Middle");
        assert!(articles[0].timestamp > articles[1].timestamp);
    }

    #[test]
    fn select_articles_orders_ties_deterministically() {
        // Feeds arrive in fetch-completion order, which varies run to run.
        // Articles with identical timestamps must still come out in the same
        // order, or downstream static sites churn on every regeneration.
        let entry_a = r#"<entry>
            <title>A</title>
            <link href="https://a.example/post"/>
            <published>2022-01-01T00:00:00Z</published>
            <summary>x</summary>
        </entry>"#;
        let entry_b = r#"<entry>
            <title>B</title>
            <link href="https://b.example/post"/>
            <published>2022-01-01T00:00:00Z</published>
            <summary>x</summary>
        </entry>"#;
        let feed_a = || feed("https://a.example/feed.xml", &atom(entry_a));
        let feed_b = || feed("https://b.example/feed.xml", &atom(entry_b));

        let one = select_unweighted(vec![feed_a(), feed_b()], 10, 10, None).unwrap();
        let two = select_unweighted(vec![feed_b(), feed_a()], 10, 10, None).unwrap();

        assert_eq!(titles(&one), titles(&two));
    }

    #[test]
    fn select_articles_drops_entry_published_exactly_at_the_cutoff() {
        use jiff::{civil::date, tz::TimeZone};

        // An article published at the very instant `before` begins is not
        // "before" that date and must be excluded.
        let cutoff_date = date(2022, 1, 1);
        let cutoff = cutoff_date
            .to_zoned(TimeZone::system())
            .unwrap()
            .timestamp();
        let feeds = vec![feed(
            FEED_URL,
            &atom(&format!(
                r#"<entry>
                    <title>At Cutoff</title>
                    <link href="https://example.com/at"/>
                    <published>{cutoff}</published>
                    <summary>x</summary>
                </entry>"#
            )),
        )];

        let articles = select_unweighted(feeds, 10, 10, Some(cutoff_date)).unwrap();
        assert!(
            articles.is_empty(),
            "boundary article was kept: {articles:?}"
        );
    }

    #[test]
    fn select_articles_drops_entries_at_or_after_before_date() {
        use jiff::civil::date;

        let feeds = vec![feed(
            FEED_URL,
            &atom(
                r#"<entry>
                    <title>Old</title>
                    <link href="https://example.com/old"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>
                <entry>
                    <title>New</title>
                    <link href="https://example.com/new"/>
                    <published>2024-01-01T00:00:00Z</published>
                    <summary>x</summary>
                </entry>"#,
            ),
        )];

        // The two-year gap dwarfs any timezone offset, so the boundary is stable.
        let articles = select_unweighted(feeds, 10, 10, Some(date(2022, 1, 1))).unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Old");
    }

    // A weighted feed only ever contributes articles among its N newest
    // eligible ones, and never more than min(per_source, N) of them.
    #[hegel::test]
    fn weighted_selection_stays_within_the_n_newest(tc: hegel::TestCase) {
        let k = tc.draw(generators::integers::<usize>().min_value(0).max_value(40));
        let per_source = tc.draw(generators::integers::<usize>().min_value(0).max_value(10));
        // Unbounded weight: a huge pool must neither panic nor hang.
        let weight = NonZeroUsize::new(tc.draw(generators::integers::<usize>().min_value(1)))
            .expect("min_value is 1");
        let mut rng = tc.draw(rand_gs::randoms());

        let weights = HashMap::from([(feed_url(), weight)]);
        let articles = select_articles(
            vec![ranked_feed(k)],
            per_source,
            usize::MAX,
            None,
            &weights,
            &mut rng,
        )
        .unwrap();

        assert!(articles.len() <= per_source.min(weight.get()));
        let mut ranks: Vec<usize> = titles(&articles)
            .iter()
            .map(|t| t.parse().expect("titles are ranks"))
            .collect();
        ranks.sort_unstable();
        let drawn = ranks.len();
        ranks.dedup();
        assert_eq!(ranks.len(), drawn, "every pick is a distinct article");
        assert!(
            ranks.iter().all(|&r| r < weight.get()),
            "picks beyond the N newest: {ranks:?}"
        );
    }

    // The slot lottery itself: distinct ranks, inside the pool, exactly
    // min(per_source, weight) of them.
    #[hegel::test]
    fn draw_weighted_slots_draws_distinct_ranks_within_the_pool(tc: hegel::TestCase) {
        let weight = NonZeroUsize::new(tc.draw(generators::integers::<usize>().min_value(1)))
            .expect("min_value is 1");
        let per_source = tc.draw(generators::integers::<usize>().min_value(0).max_value(20));
        let mut rng = tc.draw(rand_gs::randoms());

        let mut slots = draw_weighted_slots(weight, per_source, &mut rng);

        assert_eq!(slots.len(), per_source.min(weight.get()));
        assert!(slots.iter().all(|&s| s < weight.get()));
        slots.sort_unstable();
        let drawn = slots.len();
        slots.dedup();
        assert_eq!(slots.len(), drawn, "slots must be distinct");
    }

    // Weight 1 pins the pool to the single newest article, which is exactly
    // what the unweighted path picks at per_source = 1.
    #[hegel::test]
    fn weight_one_matches_unweighted_selection(tc: hegel::TestCase) {
        let k = tc.draw(generators::integers::<usize>().min_value(0).max_value(10));
        let mut rng = tc.draw(rand_gs::randoms());

        let weights = HashMap::from([(feed_url(), NonZeroUsize::new(1).expect("non-zero"))]);
        let weighted =
            select_articles(vec![ranked_feed(k)], 1, 10, None, &weights, &mut rng).unwrap();
        let unweighted = select_unweighted(vec![ranked_feed(k)], 1, 10, None).unwrap();

        assert_eq!(titles(&weighted), titles(&unweighted));
    }

    // Without weights the RNG must be inert: any two seeds give identical
    // output, so unweighted users keep deterministic builds.
    #[hegel::test]
    fn unweighted_selection_ignores_the_rng(tc: hegel::TestCase) {
        let k = tc.draw(generators::integers::<usize>().min_value(0).max_value(10));
        let per_source = tc.draw(generators::integers::<usize>().min_value(0).max_value(3));
        let num_articles = tc.draw(generators::integers::<usize>().min_value(0).max_value(5));
        let seed_a = tc.draw(generators::integers::<u64>());
        let seed_b = tc.draw(generators::integers::<u64>());

        let one = select_articles(
            vec![ranked_feed(k)],
            per_source,
            num_articles,
            None,
            &HashMap::new(),
            &mut StdRng::seed_from_u64(seed_a),
        )
        .unwrap();
        let two = select_articles(
            vec![ranked_feed(k)],
            per_source,
            num_articles,
            None,
            &HashMap::new(),
            &mut StdRng::seed_from_u64(seed_b),
        )
        .unwrap();

        assert_eq!(titles(&one), titles(&two));
    }

    // The --seed contract: a fixed seed reproduces the picks even though
    // fetches complete in arbitrary order. Fails without the URL sort in
    // select_articles.
    #[hegel::test]
    fn same_seed_reproduces_selection_regardless_of_fetch_order(tc: hegel::TestCase) {
        let seed = tc.draw(generators::integers::<u64>());
        let hosts = ["a.example", "b.example", "c.example"];
        let mut weights = HashMap::new();
        let mut entry_counts = Vec::new();
        for host in hosts {
            entry_counts.push(tc.draw(generators::integers::<usize>().min_value(0).max_value(8)));
            if let Some(w) = tc.draw(generators::optional(
                generators::integers::<usize>().min_value(1).max_value(5),
            )) {
                weights.insert(
                    Url::parse(&format!("https://{host}/feed.xml")).expect("static url"),
                    NonZeroUsize::new(w).expect("min_value is 1"),
                );
            }
        }
        let build = |counts: &[usize]| -> Vec<(Feed, Url)> {
            hosts
                .iter()
                .zip(counts)
                .map(|(host, &k)| ranked_feed_at(&format!("https://{host}/feed.xml"), k))
                .collect()
        };
        let mut backward = build(&entry_counts);
        backward.reverse();

        let one = select_articles(
            build(&entry_counts),
            1,
            10,
            None,
            &weights,
            &mut StdRng::seed_from_u64(seed),
        )
        .unwrap();
        let two = select_articles(
            backward,
            1,
            10,
            None,
            &weights,
            &mut StdRng::seed_from_u64(seed),
        )
        .unwrap();

        assert_eq!(links(&one), links(&two));
    }

    #[test]
    fn sparse_weighted_feed_sits_out_some_runs_without_warning() {
        let weights = HashMap::from([(feed_url(), NonZeroUsize::new(5).expect("non-zero"))]);

        let mut contributed = 0;
        let mut sat_out_seed = None;
        for seed in 0..200 {
            let articles = select_articles(
                vec![ranked_feed(1)],
                1,
                10,
                None,
                &weights,
                &mut StdRng::seed_from_u64(seed),
            )
            .unwrap();
            match articles.as_slice() {
                [] => sat_out_seed = Some(seed),
                [only] => {
                    assert_eq!(only.title, "0", "the only eligible article is rank 0");
                    contributed += 1;
                }
                more => panic!("one eligible article cannot yield {} picks", more.len()),
            }
        }
        // One article in a pool of five contributes on ~1/5 of seeds; both
        // outcomes are a statistical certainty across 200 seeds.
        assert!(contributed > 0, "feed never contributed");
        let sat_out_seed = sat_out_seed.expect("feed never sat out");

        let logs = warnings_logged(|| {
            let articles = select_articles(
                vec![ranked_feed(1)],
                1,
                10,
                None,
                &weights,
                &mut StdRng::seed_from_u64(sat_out_seed),
            )
            .unwrap();
            assert!(articles.is_empty());
        });
        // Sitting out the lottery is the feature working, not a feed fault.
        assert_eq!(logs, "", "sitting out must not warn");
    }

    #[test]
    fn weight_caps_contribution_below_per_source() {
        let weights = HashMap::from([(feed_url(), NonZeroUsize::new(2).expect("non-zero"))]);
        // Weight 2 with per_source 3: both slots in the pool of two are
        // always drawn, so exactly the two newest come back on any seed.
        for seed in 0..20 {
            let articles = select_articles(
                vec![ranked_feed(5)],
                3,
                10,
                None,
                &weights,
                &mut StdRng::seed_from_u64(seed),
            )
            .unwrap();
            let mut got = titles(&articles);
            got.sort_unstable();
            assert_eq!(got, ["0", "1"]);
        }
    }

    #[test]
    fn weighted_slots_count_only_qualifying_entries() {
        use jiff::civil::date;

        // The newest entry falls after --before; with weight 1 the pick must
        // be the newest qualifying entry, exactly like the unweighted path.
        let feeds = || {
            vec![feed(
                FEED_URL,
                &atom(
                    r#"<entry>
                        <title>New</title>
                        <link href="https://example.com/new"/>
                        <published>2024-01-01T00:00:00Z</published>
                        <summary>x</summary>
                    </entry>
                    <entry>
                        <title>Old</title>
                        <link href="https://example.com/old"/>
                        <published>2020-01-01T00:00:00Z</published>
                        <summary>x</summary>
                    </entry>"#,
                ),
            )]
        };
        let weights = HashMap::from([(feed_url(), NonZeroUsize::new(1).expect("non-zero"))]);
        for seed in 0..20 {
            let articles = select_articles(
                feeds(),
                1,
                10,
                Some(date(2022, 1, 1)),
                &weights,
                &mut StdRng::seed_from_u64(seed),
            )
            .unwrap();
            assert_eq!(titles(&articles), ["Old"]);
        }
    }

    // A whole run against nulled edges: files in memory, canned HTTP
    // answers, a stopped clock, and a terminal nobody watches.

    const TEMPLATE_PATH: &str = "/site/template.html";
    const URLS_PATH: &str = "/site/urls.txt";
    const TITLES_TEMPLATE: &str = "{% for a in articles %}{{ a.title }}\n{% endfor %}";

    const MOCK_RSS: &str = r#"<?xml version="1.0"?>
        <rss version="2.0">
            <channel>
                <title>Mock Feed</title>
                <link>https://example.com/</link>
                <description>desc</description>
                <item>
                    <title>Mock Article</title>
                    <link>https://example.com/mock</link>
                    <description>summary</description>
                    <pubDate>Tue, 10 Jun 2003 04:00:00 GMT</pubDate>
                </item>
            </channel>
        </rss>"#;

    fn run_args(feeds: &[&str]) -> crate::args::Args {
        crate::args::Args {
            url: feeds.iter().map(ToString::to_string).collect(),
            template_file: TEMPLATE_PATH.into(),
            no_cache: true,
            num_articles: 3,
            per_source: 1,
            ..Default::default()
        }
    }

    struct RunOutcome {
        result: crate::error::Result<()>,
        rendered: String,
        requests: Vec<crate::http::Request>,
    }

    async fn run_nulled(
        args: crate::args::Args,
        template: Option<&str>,
        urls_file: Option<&str>,
        answers: Vec<(&str, crate::http::Answer)>,
    ) -> RunOutcome {
        use super::{Deps, run_with};
        use crate::{clock::Clock, fs::Fs, http::Http, terminal::Terminal};

        let files = [(TEMPLATE_PATH, template), (URLS_PATH, urls_file)];
        let http = Http::create_null(
            answers
                .into_iter()
                .map(|(url, answer)| (Url::parse(url).unwrap(), answer)),
        );
        let requests = http.track_requests();
        let deps = Deps {
            http,
            fs: Fs::create_null(
                files
                    .into_iter()
                    .filter_map(|(path, text)| Some((path, text?))),
            ),
            clock: Clock::create_null("2026-01-01T00:00:00Z".parse().unwrap()),
            cache_path: Some("/cache/cache.json".into()),
        };
        let mut rendered = Vec::new();
        let result = run_with(&deps, &Terminal::create_null(false), args, &mut rendered).await;
        RunOutcome {
            result,
            rendered: String::from_utf8(rendered).expect("template output is UTF-8"),
            requests: requests.data(),
        }
    }

    #[tokio::test]
    async fn run_fails_on_bad_template_before_fetching_any_feed() {
        // A template with a syntax error: rendering could never succeed, so
        // the run must fail before spending time on the network.
        let outcome = run_nulled(
            run_args(&["https://feed.example/feed.xml"]),
            Some("{% for a in %}"),
            None,
            vec![],
        )
        .await;
        assert!(outcome.result.is_err());
        assert_eq!(
            outcome.requests,
            [],
            "feeds were fetched despite a template that can never render"
        );
    }

    #[tokio::test]
    async fn run_fails_when_the_template_file_is_missing() {
        let outcome = run_nulled(
            run_args(&["https://feed.example/feed.xml"]),
            None,
            None,
            vec![],
        )
        .await;
        assert!(matches!(
            outcome.result,
            Err(crate::error::OpenringError::IoError(_))
        ));
        assert_eq!(outcome.requests, []);
    }

    #[tokio::test]
    async fn run_fetches_and_renders_end_to_end() {
        let url = "https://feed.example/feed.xml";
        let outcome = run_nulled(
            run_args(&[url]),
            Some(TITLES_TEMPLATE),
            None,
            vec![(url, crate::http::Answer::ok(MOCK_RSS))],
        )
        .await;
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        assert_eq!(outcome.rendered, "Mock Article\n\n");
    }

    // The cache carries Validators from one run to the next through the
    // file system, so the second run asks "has it changed?" and renders the
    // cached feed when the server says no.
    #[tokio::test]
    async fn a_second_run_revalidates_with_what_the_first_run_cached() {
        use super::{Deps, run_with};
        use crate::{
            clock::Clock,
            fs::Fs,
            http::{Answer, Http, Request},
            terminal::Terminal,
        };

        let url = Url::parse("https://feed.example/feed.xml").unwrap();
        let http = Http::create_null([(
            url.clone(),
            vec![
                Answer::ok(MOCK_RSS).header("etag", "\"v1\""),
                Answer::status(304),
            ],
        )]);
        let requests = http.track_requests();
        let deps = Deps {
            http,
            fs: Fs::create_null([(TEMPLATE_PATH, TITLES_TEMPLATE)]),
            clock: Clock::create_null("2026-01-01T00:00:00Z".parse().unwrap()),
            cache_path: Some("/cache/cache.json".into()),
        };
        let args = || crate::args::Args {
            no_cache: false,
            // `Args::default()` keeps nothing; the binary defaults to 30 days.
            max_cache_age: std::time::Duration::from_hours(30 * 24),
            ..run_args(&[url.as_str()])
        };

        for run in ["first", "second"] {
            let mut rendered = Vec::new();
            run_with(&deps, &Terminal::create_null(false), args(), &mut rendered)
                .await
                .unwrap_or_else(|e| panic!("{run} run failed: {e}"));
            assert_eq!(String::from_utf8(rendered).unwrap(), "Mock Article\n\n");
        }
        assert_eq!(
            requests.data(),
            [
                Request {
                    url: url.clone(),
                    headers: vec![],
                },
                Request {
                    url,
                    headers: vec![("if-none-match", "\"v1\"".to_string())],
                },
            ]
        );
    }

    // Two runs over one cache: the first gets the feed, the second finds
    // the server down. Returns what the second run rendered.
    async fn run_with_server_down_the_second_time(terminal: &crate::terminal::Terminal) -> String {
        use super::{Deps, run_with};
        use crate::{
            clock::Clock,
            fs::Fs,
            http::{Answer, Http},
        };

        let url = Url::parse("https://feed.example/feed.xml").unwrap();
        let deps = Deps {
            http: Http::create_null([(
                url.clone(),
                vec![Answer::ok(MOCK_RSS), Answer::status(503)],
            )]),
            fs: Fs::create_null([(TEMPLATE_PATH, TITLES_TEMPLATE)]),
            clock: Clock::create_null("2026-01-01T00:00:00Z".parse().unwrap()),
            cache_path: Some("/cache/cache.json".into()),
        };
        let args = || crate::args::Args {
            no_cache: false,
            // `Args::default()` keeps nothing; the binary defaults to 30 days.
            max_cache_age: std::time::Duration::from_hours(30 * 24),
            ..run_args(&[url.as_str()])
        };

        let quiet = crate::terminal::Terminal::create_null(false);
        run_with(&deps, &quiet, args(), std::io::sink())
            .await
            .expect("first run");
        let mut rendered = Vec::new();
        run_with(&deps, terminal, args(), &mut rendered)
            .await
            .expect("second run");
        String::from_utf8(rendered).unwrap()
    }

    // Without a terminal, a feed served from the cache still renders, and
    // the log says why it is stale.
    #[test]
    fn a_stale_feed_renders_and_warns_when_nobody_watches() {
        let terminal = crate::terminal::Terminal::create_null(false);
        let lines = terminal.track_lines();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut rendered = String::new();
        let logs = warnings_logged(|| {
            rendered = rt.block_on(run_with_server_down_the_second_time(&terminal));
        });

        assert_eq!(rendered, "Mock Article\n\n");
        assert!(logs.contains("serving cached feed: fetch failed"), "{logs}");
        assert!(logs.contains("HTTP 503"), "{logs}");
        assert!(
            !lines.data().iter().any(|line| line.word == "Stale"),
            "a hidden bar printed the stale line too"
        );
    }

    // On a terminal, the same feed shows as one yellow Stale line and no
    // log line.
    #[test]
    fn a_stale_feed_shows_one_stale_line_when_someone_watches() {
        use crate::terminal::{Line, Terminal, Tone};

        let terminal = Terminal::create_null(true);
        let lines = terminal.track_lines();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut rendered = String::new();
        let logs = warnings_logged(|| {
            rendered = rt.block_on(run_with_server_down_the_second_time(&terminal));
        });

        assert_eq!(rendered, "Mock Article\n\n");
        assert_eq!(logs, "");
        assert_eq!(
            lines.data(),
            [Line {
                word: "Stale",
                tone: Tone::Warn,
                detail: "https://feed.example/feed.xml (The request feed at `https://feed.example/feed.xml` received an unexpected error (HTTP 503).)".to_string(),
            }]
        );
    }

    // The example template exercises every filter the docs promise (the
    // tera-contrib `date` and `striptags` we register plus tera's own
    // built-ins), so it parsing and rendering is what pins template
    // compatibility across tera upgrades.
    #[test]
    fn bundled_template_renders_an_article() {
        use super::{TEMPLATE_NAME, template_engine};

        let tera = template_engine(include_str!("../in.html")).expect("bundled template parses");
        let articles = vec![Article {
            link: Url::parse("https://example.com/post").unwrap(),
            title: "Hello World".to_string(),
            // Sanitized summaries keep safe tags and escaped entities,
            // exactly what the template's summary pipeline must flatten.
            summary: "<p>First line</p>\n<p>Second &amp; last</p>".to_string(),
            source_link: Url::parse("https://example.com/").unwrap(),
            source_title: "Example Blog".to_string(),
            timestamp: "2003-06-10T04:00:00Z".parse().unwrap(),
        }];
        let mut context = tera::Context::new();
        context.insert("articles", &articles);
        let out = tera
            .render(TEMPLATE_NAME, &context)
            .expect("bundled template renders");
        assert!(out.contains("Hello World"), "{out}");
        assert!(out.contains("June 10, 2003"), "{out}");
        // newlines_to_br and replace turn the newline into a space, and
        // striptags drops the <p> wrappers.
        assert!(out.contains("First line Second &amp; last"), "{out}");
    }

    // The bundled template doesn't use urlencode or now(), so their
    // registration needs its own coverage.
    #[test]
    fn templates_can_use_urlencode_and_now() {
        use super::{TEMPLATE_NAME, template_engine};

        let tera = template_engine(
            r#"{{ link | urlencode_strict }} {{ link | urlencode }} {{ now() | date(format="%Y") }}"#,
        )
        .expect("template parses");
        let mut context = tera::Context::new();
        context.insert("link", "https://example.com/a b");
        let out = tera.render(TEMPLATE_NAME, &context).expect("renders");

        let mut parts = out.split(' ');
        // urlencode_strict encodes every non-alphanumeric byte, `.` included.
        assert_eq!(parts.next(), Some("https%3A%2F%2Fexample%2Ecom%2Fa%20b"));
        // urlencode keeps `/` literal, matching Python's urllib quote.
        assert_eq!(parts.next(), Some("https%3A//example.com/a%20b"));
        let year = parts.next().expect("now() renders a year");
        assert!(
            year.len() == 4 && year.bytes().all(|b| b.is_ascii_digit()),
            "{year}"
        );
    }

    #[tokio::test]
    async fn run_accepts_weighted_urls_end_to_end() {
        use crate::http::Answer;

        // One healthy weighted feed plus one weighted feed that 404s: the
        // whole pipeline must accept the `URL WEIGHT` syntax and a broken
        // weighted feed must not take down the run.
        let urls_file = "https://feed.example/feed.xml 3\nhttps://feed.example/missing.xml 2\n";
        let run = || async {
            let args = crate::args::Args {
                // The same feed also arrives via -s with the same weight: the
                // CLI value and the file line must merge instead of
                // conflicting.
                url_file: Some(URLS_PATH.into()),
                seed: Some(42),
                ..run_args(&["https://feed.example/feed.xml 3"])
            };
            run_nulled(
                args,
                Some(TITLES_TEMPLATE),
                Some(urls_file),
                vec![
                    ("https://feed.example/feed.xml", Answer::ok(MOCK_RSS)),
                    ("https://feed.example/missing.xml", Answer::status(404)),
                ],
            )
            .await
        };

        let first = run().await;
        assert!(first.result.is_ok(), "{:?}", first.result);
        // The --seed contract holds through the whole pipeline, argv to
        // rendered bytes, not just inside select_articles.
        let second = run().await;
        assert_eq!(first.rendered, second.rendered);
    }

    #[tokio::test]
    async fn run_derives_summary_from_page_for_rss_feed_without_summary() {
        use crate::http::Answer;

        // The item carries no <description>, so its summary can only come
        // from the page its link points at.
        let feed = r#"<?xml version="1.0"?>
            <rss version="2.0">
                <channel>
                    <title>Mock Feed</title>
                    <link>https://feed.example/</link>
                    <description>desc</description>
                    <item>
                        <title>No Summary Article</title>
                        <link>https://feed.example/post</link>
                        <pubDate>Tue, 10 Jun 2003 04:00:00 GMT</pubDate>
                    </item>
                </channel>
            </rss>"#;
        let page = r#"<!DOCTYPE html><html><head>
            <meta name="description" content="Derived from the page itself.">
            </head><body><p>Body</p></body></html>"#;

        let outcome = run_nulled(
            run_args(&["https://feed.example/feed.xml"]),
            Some("{% for a in articles %}{{ a.summary }}{% endfor %}"),
            None,
            vec![
                ("https://feed.example/feed.xml", Answer::ok(feed)),
                (
                    "https://feed.example/post",
                    Answer::ok(page).header("content-type", "text/html"),
                ),
            ],
        )
        .await;
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        assert_eq!(outcome.rendered, "Derived from the page itself.\n");
    }

    #[tokio::test]
    async fn run_derives_summary_from_page_for_atom_feed_without_summary() {
        use crate::http::Answer;

        // The entry carries neither <summary> nor <content>, so its summary
        // can only come from the page its link points at. This mirrors real
        // Atom feeds like mitchellh.com/feed.xml.
        let feed = r#"<?xml version="1.0" encoding="utf-8"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>Mock Atom Feed</title>
                <link href="https://feed.example/"/>
                <id>urn:uuid:60a76c80-d399-11d9-b93C-0003939e0af6</id>
                <updated>2003-12-13T18:30:02Z</updated>
                <entry>
                    <title>No Summary Entry</title>
                    <link href="https://feed.example/post"/>
                    <id>urn:uuid:1225c695-cfb8-4ebb-aaaa-80da344efa6a</id>
                    <updated>2003-12-13T18:30:02Z</updated>
                </entry>
            </feed>"#;
        let page = r#"<!DOCTYPE html><html><head>
            <meta property="og:description" content="Derived for the Atom entry.">
            </head><body><p>Body</p></body></html>"#;

        let outcome = run_nulled(
            run_args(&["https://feed.example/feed.xml"]),
            Some("{% for a in articles %}{{ a.summary }}{% endfor %}"),
            None,
            vec![
                (
                    "https://feed.example/feed.xml",
                    Answer::ok(feed).header("content-type", "application/atom+xml"),
                ),
                (
                    "https://feed.example/post",
                    Answer::ok(page).header("content-type", "text/html"),
                ),
            ],
        )
        .await;
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        assert_eq!(outcome.rendered, "Derived for the Atom entry.\n");
    }

    #[tokio::test]
    async fn run_leaves_summary_empty_when_page_fetch_fails() {
        use crate::http::Answer;

        // The item's link 404s, so no summary can be derived and the run
        // must still succeed with an empty one.
        let feed = r#"<?xml version="1.0"?>
            <rss version="2.0">
                <channel>
                    <title>Mock Feed</title>
                    <link>https://feed.example/</link>
                    <description>desc</description>
                    <item>
                        <title>No Summary Article</title>
                        <link>https://feed.example/missing</link>
                        <pubDate>Tue, 10 Jun 2003 04:00:00 GMT</pubDate>
                    </item>
                </channel>
            </rss>"#;

        let outcome = run_nulled(
            run_args(&["https://feed.example/feed.xml"]),
            Some("{% for a in articles %}[{{ a.summary }}]{% endfor %}"),
            None,
            vec![
                ("https://feed.example/feed.xml", Answer::ok(feed)),
                ("https://feed.example/missing", Answer::status(404)),
            ],
        )
        .await;
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        assert_eq!(outcome.rendered, "[]\n");
    }
}
