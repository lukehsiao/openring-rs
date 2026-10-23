pub mod args;
mod cache;
mod clock;
pub mod error;
mod fan_out;
mod feed_list;
mod feedfetcher;
mod fs;
mod http;
mod nullable;
mod select;
mod source;
mod summarize;
pub mod terminal;
#[cfg(test)]
mod test_logs;

use std::io::{self, Write};
use std::path::PathBuf;

use jiff::{Timestamp, tz::TimeZone};
use rand::{SeedableRng, rngs::StdRng};
use serde::Serialize;
use tera::Tera;
use tracing::debug;
use url::Url;

use crate::{
    args::Args,
    cache::Cache,
    clock::Clock,
    error::Result,
    fan_out::{Outcome, Words, fan_out},
    feed_list::{Feed, UrlsFile},
    feedfetcher::{Fetched, fetch_feed},
    fs::Fs,
    http::Http,
    source::Source,
    terminal::Terminal,
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

const FETCH_WORDS: Words = Words {
    progress: "Fetching",
    done: "Fetched",
    stale_log: "serving cached feed: fetch failed",
    failed_log: "skipping feed: fetch failed",
    panic_log: "feed fetch task failed",
};

/// Fetch every Feed through the shared `http` client as of `now`, and
/// project each fetched document onto its Source. A Feed that is gone (404
/// or 410), or that fails with nothing cached to fall back on, is skipped.
async fn fetch_sources(
    http: &Http,
    terminal: &Terminal,
    feeds: Vec<Feed>,
    cache: &Cache,
    now: Timestamp,
) -> Vec<Source> {
    let items = feeds.into_iter().map(|f| (f.url.clone(), f)).collect();
    fan_out(terminal, &FETCH_WORDS, items, |url, feed| {
        // Both are cheap handles to shared state.
        let (http, cache) = (http.clone(), cache.clone());
        async move {
            match fetch_feed(&url, &http, &cache, now).await {
                Ok(Fetched {
                    document,
                    stale_because: None,
                }) => Outcome::Done(source::project(&document, feed)),
                Ok(Fetched {
                    document,
                    stale_because: Some(e),
                }) => Outcome::Stale(source::project(&document, feed), e.to_string()),
                Err(e) => Outcome::Failed(e.to_string()),
            }
        }
    })
    .await
}

const SUMMARY_WORDS: Words = Words {
    progress: "Summarizing",
    done: "Summary",
    stale_log: "using a fallback summary",
    failed_log: "could not derive a summary",
    panic_log: "summary fetch task failed",
};

/// Derive summaries for the chosen `articles` whose feeds provided none, by
/// fetching each article's own page (see [`summarize::fetch_summary`]).
///
/// Runs after selection so only the articles that will render trigger a page
/// fetch, and mutates `articles` in place. An article whose page yields
/// nothing keeps its empty summary, exactly as if the feed had one that was
/// blank, so nothing here can fail the run.
async fn fill_missing_summaries(http: &Http, terminal: &Terminal, articles: &mut [Article]) {
    let items = articles
        .iter()
        .enumerate()
        .filter(|(_, a)| a.summary.is_empty())
        .map(|(i, a)| (a.link.clone(), i))
        .collect();
    let summaries = fan_out(terminal, &SUMMARY_WORDS, items, |url, i| {
        let http = http.clone();
        async move {
            // A page with nothing quotable is routine, not worth a report.
            summarize::fetch_summary(&http, &url)
                .await
                .map_or(Outcome::Skipped, |summary| Outcome::Done((i, summary)))
        }
    })
    .await;
    for (i, summary) in summaries {
        articles[i].summary = summary;
    }
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

    // The Feed list from -s values and the urls file. Repeated listings
    // collapse and contradictory weights fail fast, before any fetch.
    let urls_text = match &args.url_file {
        Some(path) => Some(deps.fs.read_to_string(path)?),
        None => None,
    };
    let feed_list = feed_list::parse(
        &args.url,
        args.url_file
            .as_deref()
            .zip(urls_text.as_deref())
            .map(|(path, text)| UrlsFile { path, text }),
    )?;
    // The cutoff as an instant, computed once: midnight at the start of
    // `before` in the system timezone.
    let cutoff = args
        .before
        .map(|date| date.to_zoned(TimeZone::system()).map(|z| z.timestamp()))
        .transpose()?;

    let sources = fetch_sources(&deps.http, terminal, feed_list, &cache, now).await;

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
    let policy = select::Policy {
        per_source: args.per_source,
        num_articles: args.num_articles,
        cutoff,
    };
    let mut articles = select::select(sources, &policy, &mut rng);

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

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{Article, write_output};
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

    // --before reaches selection: only articles dated before that day
    // render. The dates sit years apart, so the system timezone that sets
    // midnight cannot move either across the cutoff.
    #[tokio::test]
    async fn run_renders_only_articles_before_the_before_date() {
        let url = "https://feed.example/feed.xml";
        let feed = r#"<?xml version="1.0"?>
            <rss version="2.0"><channel><title>t</title><link>https://feed.example/</link>
                <item><title>Old</title><link>https://feed.example/old</link>
                    <pubDate>Wed, 01 Jan 2020 00:00:00 GMT</pubDate></item>
                <item><title>New</title><link>https://feed.example/new</link>
                    <pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate></item>
            </channel></rss>"#;
        let args = crate::args::Args {
            before: Some(jiff::civil::date(2022, 1, 1)),
            per_source: 10,
            ..run_args(&[url])
        };
        let outcome = run_nulled(
            args,
            Some(TITLES_TEMPLATE),
            None,
            vec![(url, crate::http::Answer::ok(feed))],
        )
        .await;
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        assert_eq!(outcome.rendered, "Old\n\n");
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

    // A hostile feed cannot get script into the page the bundled template
    // renders, through links, titles, or summaries.
    #[tokio::test]
    async fn a_hostile_feed_cannot_inject_script_through_the_bundled_template() {
        use crate::http::Answer;

        let feed = r#"<?xml version="1.0" encoding="utf-8"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>Evil &lt;script&gt;alert(1)&lt;/script&gt;</title>
                <link rel="alternate" href='javascript:"><img src=x onerror=alert(1)>'/>
                <updated>2020-01-01T00:00:00Z</updated>
                <entry>
                    <title>Bad link</title>
                    <link href='javascript:"><img src=x onerror=alert(1)>'/>
                    <published>2020-01-02T00:00:00Z</published>
                    <summary>s</summary>
                </entry>
                <entry>
                    <title>&lt;img src=x onerror=alert(2)&gt;Good</title>
                    <link href="https://feed.example/good"/>
                    <published>2020-01-01T00:00:00Z</published>
                    <summary>&lt;script&gt;alert(3)&lt;/script&gt;fine</summary>
                </entry>
            </feed>"#;
        let outcome = run_nulled(
            run_args(&["https://feed.example/feed.xml"]),
            Some(include_str!("../in.html")),
            None,
            vec![("https://feed.example/feed.xml", Answer::ok(feed))],
        )
        .await;

        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        let page = outcome.rendered.to_ascii_lowercase();
        for live in ["javascript:", "<script", "<img", "onerror"] {
            assert!(!page.contains(live), "{live} reached the page:\n{page}");
        }
        assert!(page.contains("https://feed.example/good"), "{page}");
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
        // rendered bytes, not just inside select::select.
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
