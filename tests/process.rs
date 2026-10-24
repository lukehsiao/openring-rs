//! Behavior only a real `openring` process shows: how `main` sets up logs
//! off a terminal, what happens on a real closed pipe, and how the binary
//! copes with a real limit on file descriptors. Every test serves feeds from
//! a local mock server, never the network.
//!
//! Each test uses a multi-threaded runtime, so the mock server keeps serving
//! while the test thread blocks on the child process.

use std::{
    io::Write,
    process::{Command, Stdio},
};

use tempfile::NamedTempFile;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

const RSS_BODY: &str = r#"<?xml version="1.0"?>
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

/// A mock server that gives every request the same answer, and a template
/// that prints one `X` per Article.
struct Fixture {
    server: MockServer,
    template: NamedTempFile,
}

impl Fixture {
    async fn start(answer: ResponseTemplate) -> Self {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(answer)
            .mount(&server)
            .await;
        let mut template = NamedTempFile::new().unwrap();
        template
            .write_all(b"{% for a in articles %}X{% endfor %}")
            .unwrap();
        Self { server, template }
    }

    fn feed_url(&self, path: &str) -> String {
        format!("{}{path}", self.server.uri())
    }

    /// The arguments every test passes: the template, and no cache, so a
    /// run never reads or writes the user's real cache.
    fn args(&self) -> [&str; 3] {
        ["-t", self.template.path().to_str().unwrap(), "--no-cache"]
    }
}

// When stdout closes before openring writes its output (e.g. `openring ...
// | head`), the binary must exit cleanly rather than panic on EPIPE.
#[tokio::test(flavor = "multi_thread")]
async fn exits_cleanly_when_stdout_is_closed() {
    let fixture = Fixture::start(ResponseTemplate::new(200).set_body_string(RSS_BODY)).await;

    let mut child = Command::new(env!("CARGO_BIN_EXE_openring"))
        .args(fixture.args())
        .args(["-s", &fixture.feed_url("/feed.xml")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Close the read end of the pipe before the binary gets to its output, so
    // its write hits EPIPE.
    drop(child.stdout.take());

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "expected clean exit on closed stdout, got {:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

// Outside a terminal the progress bar is hidden, and its lines vanish with
// it. A feed that fails to fetch must still be reported, or cron jobs and CI
// site builds lose it without a word.
#[tokio::test(flavor = "multi_thread")]
async fn warns_on_stderr_when_a_feed_fails_without_a_terminal() {
    let fixture = Fixture::start(ResponseTemplate::new(500)).await;
    let feed_url = fixture.feed_url("/feed.xml");

    // A piped stderr is never a terminal, so the bar is hidden no matter
    // where the test suite itself runs.
    let output = Command::new(env!("CARGO_BIN_EXE_openring"))
        .args(fixture.args())
        .args(["-s", &feed_url])
        // tracing paints log lines unless NO_COLOR is set, so leave it unset
        // to prove the binary turns color off by itself off a terminal.
        .env_remove("NO_COLOR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a failed feed does not fail the run\nstderr:\n{stderr}"
    );
    assert!(stderr.contains("WARN"), "{stderr}");
    assert!(stderr.contains(&feed_url), "{stderr}");
    assert!(stderr.contains("HTTP 500"), "{stderr}");
    assert!(
        !stderr.contains('\u{1b}'),
        "escape codes off a terminal: {stderr:?}"
    );
}

// Fetch concurrency must be bounded: a long urls file otherwise opens one
// socket per feed at once and runs out of file descriptors (macOS defaults
// to 256), silently dropping feeds. `ulimit` needs a POSIX shell.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn fetches_every_feed_under_a_tight_fd_limit() {
    // Well above the descriptor limit below, so an unbounded client fails.
    const FEED_COUNT: usize = 100;

    // The delay forces the fetches to overlap, so an unbounded client holds
    // every socket open at once.
    let fixture = Fixture::start(
        ResponseTemplate::new(200)
            .set_body_string(RSS_BODY)
            .set_delay(std::time::Duration::from_millis(100)),
    )
    .await;
    let mut url_file = NamedTempFile::new().unwrap();
    for i in 0..FEED_COUNT {
        writeln!(url_file, "{}", fixture.feed_url(&format!("/feed/{i}"))).unwrap();
    }

    // Run the real binary with its fd limit squeezed well below FEED_COUNT.
    let output = Command::new("sh")
        .args([
            "-c",
            "ulimit -n 64; exec \"$0\" \"$@\"",
            env!("CARGO_BIN_EXE_openring"),
        ])
        .args(fixture.args())
        // More slots than feeds, so every fetched feed shows in the output.
        .args(["-S", url_file.path().to_str().unwrap(), "-n", "200"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let fetched = stdout.trim().chars().filter(|c| *c == 'X').count();
    assert_eq!(
        fetched,
        FEED_COUNT,
        "feeds were dropped under the fd limit\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
