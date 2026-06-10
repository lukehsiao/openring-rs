//! When stdout closes before openring writes its output (e.g. `openring ... |
//! head`), the binary must exit cleanly rather than panicking on EPIPE.

use std::{
    io::Write,
    process::{Command, Stdio},
};

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

// Multi-threaded runtime so the mock server keeps serving while this thread
// blocks in `wait_with_output`.
#[tokio::test(flavor = "multi_thread")]
async fn exits_cleanly_when_stdout_is_closed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RSS_BODY))
        .mount(&server)
        .await;

    let mut template = tempfile::NamedTempFile::new().unwrap();
    template
        .write_all(b"{% for a in articles %}{{ a.title }}\n{% endfor %}")
        .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_openring"))
        .args([
            "-s",
            &server.uri(),
            "-t",
            template.path().to_str().unwrap(),
            "--no-cache",
        ])
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
