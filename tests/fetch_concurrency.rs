//! Fetch concurrency must be bounded: a long urls file otherwise opens one
//! socket per feed simultaneously and exhausts the process's file
//! descriptors (macOS defaults to 256), silently dropping feeds.

use std::{
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

const FEED_COUNT: usize = 100;

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

#[tokio::test(flavor = "multi_thread")]
async fn fetches_every_feed_under_a_tight_fd_limit() {
    let server = MockServer::start().await;
    // The delay forces the fetches to overlap, so an unbounded client holds
    // every socket open at once.
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(RSS_BODY)
                .set_delay(Duration::from_millis(100)),
        )
        .mount(&server)
        .await;

    let mut url_file = tempfile::NamedTempFile::new().unwrap();
    for i in 0..FEED_COUNT {
        writeln!(url_file, "{}/feed/{i}", server.uri()).unwrap();
    }

    let mut template = tempfile::NamedTempFile::new().unwrap();
    template
        .write_all(b"{% for a in articles %}X{% endfor %}")
        .unwrap();

    // Run the real binary with its fd limit squeezed well below FEED_COUNT.
    let output = Command::new("sh")
        .args([
            "-c",
            "ulimit -n 64; exec \"$0\" \"$@\"",
            env!("CARGO_BIN_EXE_openring"),
            "-S",
            url_file.path().to_str().unwrap(),
            "-t",
            template.path().to_str().unwrap(),
            "--no-cache",
            "-n",
            "200",
        ])
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
