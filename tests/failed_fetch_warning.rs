//! Outside a terminal the progress bar is hidden, and its lines vanish with
//! it. A feed that fails to fetch must still be reported, or cron jobs and CI
//! site builds lose it without a word.

use std::{
    io::Write,
    process::{Command, Stdio},
};

use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

// Multi-threaded runtime so the mock server keeps serving while this thread
// blocks in `output`.
#[tokio::test(flavor = "multi_thread")]
async fn warns_on_stderr_when_a_feed_fails_without_a_terminal() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let feed_url = format!("{}/feed.xml", server.uri());

    let mut template = tempfile::NamedTempFile::new().unwrap();
    template
        .write_all(b"{% for a in articles %}{{ a.title }}\n{% endfor %}")
        .unwrap();

    // A piped stderr is never a terminal, so the bar is hidden no matter
    // where the test suite itself runs.
    let output = Command::new(env!("CARGO_BIN_EXE_openring"))
        .args([
            "-s",
            &feed_url,
            "-t",
            template.path().to_str().unwrap(),
            "--no-cache",
        ])
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
}
