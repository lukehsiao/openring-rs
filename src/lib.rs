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
mod test_support;

use std::io::{self, Write};
use std::path::PathBuf;

use rand::RngExt;
use tera::Tera;
use tracing::debug;

use crate::{
    args::Args,
    cache::Cache,
    clock::Clock,
    error::{OpenringError, Result},
    feed_list::UrlsFile,
    fs::Fs,
    http::Http,
    terminal::Terminal,
};

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
    let tera = template_engine(&read_file(&deps.fs, &args.template_file)?)?;

    // One instant for the whole run, so cache pruning and every fetch agree
    // on which entries are too old and which retry windows are open.
    let now = deps.clock.now();
    let cache = Cache::open(
        &deps.fs,
        deps.cache_path.as_deref().filter(|_| !args.no_cache),
        args.max_cache_age,
        now,
    );

    // The Feed list from -s values and the urls file. Repeated listings
    // collapse and contradictory weights fail fast, before any fetch.
    let urls_text = args
        .url_file
        .as_deref()
        .map(|path| read_file(&deps.fs, path))
        .transpose()?;
    let feed_list = feed_list::parse(
        &args.url,
        args.url_file
            .as_deref()
            .zip(urls_text.as_deref())
            .map(|(path, text)| UrlsFile { path, text }),
    )?;
    // The cutoff as an instant, computed once: the start of the `before`
    // date in the clock's time zone.
    let cutoff = args
        .before
        .map(|date| deps.clock.start_of(date))
        .transpose()
        .map_err(OpenringError::CivilDateError)?;

    let sources = feedfetcher::fetch_sources(&deps.http, terminal, feed_list, &cache, now).await;

    cache.save(&deps.fs);

    // Entropy by default: rotating the weighted picks between runs is the
    // point. A fixed seed reproduces the same picks for tests and stable
    // site builds.
    let seed = args.seed.unwrap_or_else(|| rand::rng().random());
    let policy = select::Policy {
        per_source: args.per_source,
        num_articles: args.num_articles,
        cutoff,
    };
    let mut articles = select::select(sources, &policy, seed);

    // Feeds that ship no summary get one derived from the article page
    // itself. Deferred until here so only the articles that will render
    // trigger a page fetch.
    summarize::fill_missing_summaries(&deps.http, terminal, &mut articles).await;

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
        result => result.map_err(OpenringError::OutputError),
    }
}

/// Read a file the user named, with the path in the error if it fails.
fn read_file(fs: &Fs, path: &std::path::Path) -> Result<String> {
    fs.read_to_string(path)
        .map_err(|source| OpenringError::ReadFileError {
            path: path.display().to_string(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::write_output;
    use crate::test_support::{block_on, warnings_logged};

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
            Err(crate::error::OpenringError::OutputError(_))
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

    // Deps over a nulled Http, a file system holding `files`, a frozen
    // clock, and one cache file that later runs share.
    fn nulled_deps<'a>(
        http: crate::http::Http,
        files: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> super::Deps {
        use crate::{clock::Clock, fs::Fs};

        super::Deps {
            http,
            fs: Fs::create_null(files),
            clock: Clock::create_null(
                "2026-01-01T00:00:00Z".parse().unwrap(),
                jiff::tz::TimeZone::UTC,
            ),
            cache_path: Some("/cache/cache.json".into()),
        }
    }

    async fn run_nulled(
        args: crate::args::Args,
        template: Option<&str>,
        urls_file: Option<&str>,
        answers: Vec<(&str, crate::http::Answer)>,
    ) -> RunOutcome {
        use super::run_with;
        use crate::{http::Http, terminal::Terminal};

        let files = [(TEMPLATE_PATH, template), (URLS_PATH, urls_file)];
        let http = Http::create_null(
            answers
                .into_iter()
                .map(|(url, answer)| (Url::parse(url).unwrap(), answer)),
        );
        let requests = http.track_requests();
        let deps = nulled_deps(
            http,
            files
                .into_iter()
                .filter_map(|(path, text)| Some((path, text?))),
        );
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
    async fn run_names_the_template_file_it_cannot_read() {
        let outcome = run_nulled(
            run_args(&["https://feed.example/feed.xml"]),
            None,
            None,
            vec![],
        )
        .await;
        let err = outcome.result.expect_err("no template to read");
        assert_eq!(
            err.to_string(),
            format!("Failed to read `{TEMPLATE_PATH}`.")
        );
        assert_eq!(outcome.requests, []);
    }

    #[tokio::test]
    async fn run_names_the_urls_file_it_cannot_read() {
        let args = crate::args::Args {
            url_file: Some(URLS_PATH.into()),
            ..run_args(&[])
        };
        let outcome = run_nulled(args, Some(TITLES_TEMPLATE), None, vec![]).await;
        let err = outcome.result.expect_err("no urls file to read");
        assert_eq!(err.to_string(), format!("Failed to read `{URLS_PATH}`."));
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

    // --before reaches selection as the first instant of that date in the
    // clock's time zone, and an Article at exactly that instant is out.
    // UTC would let both Articles through, so the test runs at UTC+9.
    #[tokio::test]
    async fn run_renders_only_articles_before_the_start_of_the_before_date() {
        use super::run_with;
        use crate::{
            clock::Clock,
            http::{Answer, Http},
            terminal::Terminal,
        };

        let url = "https://feed.example/feed.xml";
        // Midnight on 2024-01-02 at UTC+9 is 15:00 UTC the day before.
        let feed = r#"<?xml version="1.0"?>
            <rss version="2.0"><channel><title>t</title><link>https://feed.example/</link>
                <item><title>Last</title><link>https://feed.example/last</link>
                    <pubDate>Mon, 01 Jan 2024 14:59:59 GMT</pubDate></item>
                <item><title>Out</title><link>https://feed.example/out</link>
                    <pubDate>Mon, 01 Jan 2024 15:00:00 GMT</pubDate></item>
            </channel></rss>"#;
        let http = Http::create_null([(Url::parse(url).unwrap(), Answer::ok(feed))]);
        let mut deps = nulled_deps(http, [(TEMPLATE_PATH, TITLES_TEMPLATE)]);
        deps.clock = Clock::create_null(
            deps.clock.now(),
            jiff::tz::TimeZone::fixed(jiff::tz::offset(9)),
        );
        let args = crate::args::Args {
            before: Some(jiff::civil::date(2024, 1, 2)),
            per_source: 10,
            ..run_args(&[url])
        };

        let mut rendered = Vec::new();
        run_with(&deps, &Terminal::create_null(false), args, &mut rendered)
            .await
            .expect("run succeeds");
        assert_eq!(String::from_utf8(rendered).unwrap(), "Last\n\n");
    }

    fn cached_args(url: &Url) -> crate::args::Args {
        crate::args::Args {
            no_cache: false,
            // `Args::default()` keeps nothing; the binary defaults to 30 days.
            max_cache_age: std::time::Duration::from_hours(30 * 24),
            ..run_args(&[url.as_str()])
        }
    }

    // The cache carries Validators from one run to the next through the
    // file system, so the second run asks "has it changed?" and renders the
    // cached feed when the server says no.
    #[tokio::test]
    async fn a_second_run_revalidates_with_what_the_first_run_cached() {
        use super::run_with;
        use crate::{
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
        let deps = nulled_deps(http, [(TEMPLATE_PATH, TITLES_TEMPLATE)]);

        for run in ["first", "second"] {
            let mut rendered = Vec::new();
            run_with(
                &deps,
                &Terminal::create_null(false),
                cached_args(&url),
                &mut rendered,
            )
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

    // --no-cache neither reads the cache file nor writes it. A cached run
    // before or after it learns no Validators from it, and it uses none.
    #[tokio::test]
    async fn a_no_cache_run_neither_reads_nor_writes_the_cache() {
        use super::run_with;
        use crate::{
            http::{Answer, Http},
            terminal::Terminal,
        };

        let url = Url::parse("https://feed.example/feed.xml").unwrap();
        let no_cache = || run_args(&[url.as_str()]);
        let tagged = || Answer::ok(MOCK_RSS).header("etag", "\"v1\"");
        for runs in [
            [cached_args(&url), no_cache()],
            [no_cache(), cached_args(&url)],
        ] {
            let http = Http::create_null([(url.clone(), vec![tagged(), tagged()])]);
            let requests = http.track_requests();
            let deps = nulled_deps(http, [(TEMPLATE_PATH, TITLES_TEMPLATE)]);
            for args in runs {
                run_with(&deps, &Terminal::create_null(false), args, std::io::sink())
                    .await
                    .expect("run succeeds");
            }
            let headers: Vec<_> = requests.data().into_iter().map(|r| r.headers).collect();
            assert_eq!(headers, [vec![], vec![]]);
        }
    }

    // Two runs over one cache: the first gets the feed, the second finds
    // the server down. Returns what the second run rendered and everything
    // either run logged.
    fn run_with_server_down_the_second_time(
        terminal: &crate::terminal::Terminal,
    ) -> (String, String) {
        use super::run_with;
        use crate::{
            http::{Answer, Http},
            terminal::Terminal,
        };

        let url = Url::parse("https://feed.example/feed.xml").unwrap();
        let http =
            Http::create_null([(url.clone(), vec![Answer::ok(MOCK_RSS), Answer::status(503)])]);
        let deps = nulled_deps(http, [(TEMPLATE_PATH, TITLES_TEMPLATE)]);
        let mut rendered = Vec::new();
        let logs = warnings_logged(|| {
            block_on(async {
                let quiet = Terminal::create_null(false);
                run_with(&deps, &quiet, cached_args(&url), std::io::sink())
                    .await
                    .expect("first run");
                run_with(&deps, terminal, cached_args(&url), &mut rendered)
                    .await
                    .expect("second run");
            });
        });
        (String::from_utf8(rendered).unwrap(), logs)
    }

    // Without a terminal, a feed served from the cache still renders, and
    // the log says why it is stale.
    #[test]
    fn a_stale_feed_renders_and_warns_when_nobody_watches() {
        let terminal = crate::terminal::Terminal::create_null(false);
        let lines = terminal.track_lines();
        let (rendered, logs) = run_with_server_down_the_second_time(&terminal);

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
        let (rendered, logs) = run_with_server_down_the_second_time(&terminal);

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
    #[tokio::test]
    async fn bundled_template_renders_an_article() {
        use crate::http::Answer;

        // The summary keeps safe tags and escaped entities after
        // sanitizing, exactly what the template's summary pipeline must
        // flatten.
        let feed = r#"<?xml version="1.0"?>
            <rss version="2.0"><channel><title>Example Blog</title>
                <link>https://example.com/</link>
                <item><title>Hello World</title><link>https://example.com/post</link>
                    <description><![CDATA[<p>First line</p>
<p>Second &amp; last</p>]]></description>
                    <pubDate>Tue, 10 Jun 2003 04:00:00 GMT</pubDate></item>
            </channel></rss>"#;
        let outcome = run_nulled(
            run_args(&["https://feed.example/feed.xml"]),
            Some(include_str!("../in.html")),
            None,
            vec![("https://feed.example/feed.xml", Answer::ok(feed))],
        )
        .await;

        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        let out = outcome.rendered;
        assert!(out.contains("Hello World"), "{out}");
        assert!(out.contains("June 10, 2003"), "{out}");
        // newlines_to_br and replace turn the newline into a space, and
        // striptags drops the <p> wrappers.
        assert!(out.contains("First line Second &amp; last"), "{out}");
    }

    // A summary derived from a hostile page shows its markup as text, even
    // through `| safe`, which the README allows for element content.
    #[tokio::test]
    async fn a_hostile_page_cannot_inject_script_through_a_derived_summary() {
        use crate::http::Answer;

        let feed = r#"<?xml version="1.0"?>
            <rss version="2.0"><channel><title>t</title><link>https://feed.example/</link>
                <item><title>No summary</title><link>https://feed.example/post</link>
                    <pubDate>Tue, 10 Jun 2003 04:00:00 GMT</pubDate></item>
            </channel></rss>"#;
        let page = r#"<html><head>
            <meta name="description" content="&lt;script&gt;alert(1)&lt;/script&gt; &amp; more">
            </head></html>"#;
        let outcome = run_nulled(
            run_args(&["https://feed.example/feed.xml"]),
            Some("{% for a in articles %}{{ a.summary | safe }}{% endfor %}"),
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
        assert_eq!(
            outcome.rendered,
            "&lt;script&gt;alert(1)&lt;/script&gt; &amp; more\n"
        );
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

    // Only Articles that render and lack a summary send a page request. A
    // feed summary is used as is, and an Article that selection drops is
    // never summarized.
    #[tokio::test]
    async fn run_fetches_pages_only_for_selected_articles_missing_a_summary() {
        use crate::http::Answer;

        // Each derived summary must land on its own Article, even when an
        // earlier page yields nothing.
        let feed = r#"<?xml version="1.0"?>
            <rss version="2.0"><channel><title>t</title><link>https://feed.example/</link>
                <item><title>Has summary</title><link>https://feed.example/has</link>
                    <description>From the feed.</description>
                    <pubDate>Thu, 04 Jan 2024 00:00:00 GMT</pubDate></item>
                <item><title>Page fails</title><link>https://feed.example/fails</link>
                    <pubDate>Wed, 03 Jan 2024 00:00:00 GMT</pubDate></item>
                <item><title>Page works</title><link>https://feed.example/works</link>
                    <pubDate>Tue, 02 Jan 2024 00:00:00 GMT</pubDate></item>
                <item><title>Dropped</title><link>https://feed.example/dropped</link>
                    <pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate></item>
            </channel></rss>"#;
        let page =
            r#"<html><head><meta name="description" content="From the page."></head></html>"#;
        let args = crate::args::Args {
            num_articles: 3,
            per_source: 10,
            ..run_args(&["https://feed.example/feed.xml"])
        };
        let outcome = run_nulled(
            args,
            Some("{% for a in articles %}{{ a.title }}: {{ a.summary }}\n{% endfor %}"),
            None,
            vec![
                ("https://feed.example/feed.xml", Answer::ok(feed)),
                ("https://feed.example/fails", Answer::status(404)),
                ("https://feed.example/works", Answer::ok(page)),
            ],
        )
        .await;

        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        assert_eq!(
            outcome.rendered,
            "Has summary: From the feed.\nPage fails: \nPage works: From the page.\n\n"
        );
        let mut urls: Vec<&str> = outcome.requests.iter().map(|r| r.url.as_str()).collect();
        // Page fetches run concurrently, so their order is not fixed.
        urls.sort_unstable();
        assert_eq!(
            urls,
            [
                "https://feed.example/fails",
                "https://feed.example/feed.xml",
                "https://feed.example/works"
            ]
        );
    }

    #[tokio::test]
    async fn run_leaves_summary_empty_when_page_fetch_fails() {
        use crate::http::Answer;

        // The item's link 404s, so no summary can be derived and the run
        // must still succeed with an empty one. Filters such as
        // `newlines_to_br` reject a null, so the empty summary must reach
        // the template as a string.
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
            Some("{% for a in articles %}[{{ a.summary | newlines_to_br }}]{% endfor %}"),
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
