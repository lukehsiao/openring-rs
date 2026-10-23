//! The HTTP edge. Every request openring makes goes through [`Http`], and
//! this is the only module that touches `reqwest`. Tests swap `reqwest` for
//! an embedded stub with `Http::create_null`, so everything above the
//! stub runs for real without a network.

use std::time::Duration;

pub(crate) use reqwest::StatusCode;
use reqwest::{ClientBuilder, header::HeaderMap};
use thiserror::Error;
use url::Url;

use crate::nullable::OutputListener;

/// How openring identifies itself to servers.
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// A request could not be built, sent, or read to the end. The message
/// carries the whole chain of causes, such as "error sending request for
/// url (…): client error (Connect): tcp connect error: Connection refused".
#[derive(Debug, Error)]
#[error("{0}")]
pub struct TransportError(String);

impl From<reqwest::Error> for TransportError {
    fn from(e: reqwest::Error) -> Self {
        use std::{error::Error as _, fmt::Write as _};

        // reqwest's own message names only what failed; why it failed lives
        // in the source chain.
        let mut text = e.to_string();
        let mut cause = e.source();
        while let Some(c) = cause {
            let _ = write!(text, ": {c}");
            cause = c.source();
        }
        Self(text)
    }
}

/// Why [`Response::read_capped`] gave up on a body.
#[derive(Debug, Error)]
pub(crate) enum ReadError {
    /// The body grew past the limit. `bytes` is how much had arrived when
    /// reading stopped, so it is always more than the limit.
    #[error("the body grew past the limit ({bytes} bytes)")]
    TooLarge { bytes: u64 },
    #[error(transparent)]
    Transfer(#[from] TransportError),
}

/// One request as a test sees it go out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) url: Url,
    pub(crate) headers: Vec<(&'static str, String)>,
}

/// One client for the whole run: one connection pool, one TLS setup, and
/// the openring user agent. Clones share all of it.
#[derive(Debug, Clone)]
pub(crate) struct Http {
    client: Client,
    requests: OutputListener<Request>,
}

impl Http {
    /// The real client.
    ///
    /// Timeouts are granular rather than one total deadline: a large feed on
    /// a slow server (e.g. 1.7 MiB at ~85 KiB/s) is legitimate and must be
    /// allowed to finish, while a dead or stalled server should fail fast.
    pub(crate) fn create() -> Result<Self, TransportError> {
        let client = ClientBuilder::new()
            // A server that cannot complete a TCP/TLS handshake in 10s is down.
            .connect_timeout(Duration::from_secs(10))
            // A transfer may take as long as it keeps flowing, but 30s with
            // no bytes at all means a stall.
            .read_timeout(Duration::from_secs(30))
            // Ceiling so a trickling server cannot pin a fetch slot forever.
            .timeout(Duration::from_mins(5))
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self {
            client: Client::Real(client),
            requests: OutputListener::default(),
        })
    }

    /// GET `url` with the given extra `headers`, following redirects.
    pub(crate) async fn get(
        &self,
        url: &Url,
        headers: &[(&'static str, &str)],
    ) -> Result<Response, TransportError> {
        self.requests.emit(|| Request {
            url: url.clone(),
            headers: headers
                .iter()
                .map(|&(name, value)| (name, value.to_string()))
                .collect(),
        });
        Ok(Response {
            raw: self.client.get(url, headers).await?,
        })
    }
}

/// A response whose status and headers have arrived. The body arrives
/// through [`Response::read_capped`].
#[derive(Debug)]
pub(crate) struct Response {
    raw: RawResponse,
}

impl Response {
    pub(crate) fn status(&self) -> StatusCode {
        self.raw.status()
    }

    /// The URL that answered, after any redirects.
    pub(crate) fn url(&self) -> &Url {
        self.raw.url()
    }

    /// A header's value, if present and printable.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.raw.headers().get(name).and_then(|v| v.to_str().ok())
    }

    /// The length the server announced. Chunked and compressed responses
    /// announce none.
    pub(crate) fn content_length(&self) -> Option<u64> {
        self.raw.content_length()
    }

    /// Read the body, giving up once it grows past `limit` bytes. The cap
    /// applies to the bytes as they arrive, because Content-Length is absent
    /// or describes the compressed size for chunked and compressed bodies.
    pub(crate) async fn read_capped(mut self, limit: usize) -> Result<Vec<u8>, ReadError> {
        let mut body = Vec::new();
        while self.raw.chunk_into(&mut body).await? {
            if body.len() > limit {
                return Err(ReadError::TooLarge {
                    bytes: u64::try_from(body.len()).unwrap_or(u64::MAX),
                });
            }
        }
        Ok(body)
    }
}

// The seam. Each type below mirrors only the reqwest calls this module
// makes, and its real arm only forwards to reqwest.

#[derive(Debug, Clone)]
enum Client {
    Real(reqwest::Client),
    #[cfg(test)]
    Stubbed(stub::StubbedClient),
}

impl Client {
    async fn get(
        &self,
        url: &Url,
        headers: &[(&'static str, &str)],
    ) -> Result<RawResponse, TransportError> {
        match self {
            Self::Real(client) => {
                let mut request = client.get(url.clone());
                for &(name, value) in headers {
                    request = request.header(name, value);
                }
                Ok(RawResponse::Real(request.send().await?))
            }
            #[cfg(test)]
            Self::Stubbed(client) => client.get(url).map(RawResponse::Stubbed),
        }
    }
}

#[derive(Debug)]
enum RawResponse {
    Real(reqwest::Response),
    #[cfg(test)]
    Stubbed(stub::StubbedResponse),
}

impl RawResponse {
    fn status(&self) -> StatusCode {
        match self {
            Self::Real(response) => response.status(),
            #[cfg(test)]
            Self::Stubbed(response) => response.status,
        }
    }

    fn url(&self) -> &Url {
        match self {
            Self::Real(response) => response.url(),
            #[cfg(test)]
            Self::Stubbed(response) => &response.url,
        }
    }

    fn headers(&self) -> &HeaderMap {
        match self {
            Self::Real(response) => response.headers(),
            #[cfg(test)]
            Self::Stubbed(response) => &response.headers,
        }
    }

    fn content_length(&self) -> Option<u64> {
        match self {
            Self::Real(response) => response.content_length(),
            #[cfg(test)]
            Self::Stubbed(response) => response.content_length,
        }
    }

    /// Append the next chunk of the body to `body`, or return `false` once
    /// the body has ended.
    async fn chunk_into(&mut self, body: &mut Vec<u8>) -> Result<bool, TransportError> {
        let chunk = match self {
            Self::Real(response) => response.chunk().await?,
            #[cfg(test)]
            Self::Stubbed(response) => response.chunk()?.map(Into::into),
        };
        Ok(chunk.map(|c| body.extend_from_slice(&c)).is_some())
    }
}

#[cfg(test)]
pub(crate) use stub::Answer;

#[cfg(test)]
impl Http {
    /// A client that answers from `answers`, keyed by request URL, and
    /// never touches the network. A URL with no answer gets a loud 503.
    pub(crate) fn create_null<A>(answers: impl IntoIterator<Item = (Url, A)>) -> Self
    where
        A: Into<crate::nullable::ConfigurableResponses<Answer>>,
    {
        Self {
            client: Client::Stubbed(stub::StubbedClient::new(answers)),
            requests: OutputListener::default(),
        }
    }

    /// Record every request made from now on, through any clone.
    pub(crate) fn track_requests(&self) -> crate::nullable::OutputTracker<Request> {
        self.requests.track()
    }
}

#[cfg(test)]
mod stub {
    use std::{
        collections::{HashMap, VecDeque},
        sync::{Arc, Mutex, PoisonError},
    };

    use reqwest::{
        StatusCode,
        header::{HeaderMap, HeaderName, HeaderValue},
    };
    use url::Url;

    use super::TransportError;
    use crate::nullable::ConfigurableResponses;

    /// What the stubbed network answers to one request.
    #[derive(Debug, Clone)]
    pub(crate) struct Answer {
        status: StatusCode,
        headers: Vec<(HeaderName, HeaderValue)>,
        chunks: Vec<Vec<u8>>,
        content_length: Option<u64>,
        chunked: bool,
        redirected_to: Option<Url>,
        send_error: Option<String>,
        transfer_error: Option<String>,
    }

    impl Answer {
        /// A response with this status and an empty body.
        pub(crate) fn status(code: u16) -> Self {
            Self {
                status: StatusCode::from_u16(code).expect("a valid status code"),
                headers: Vec::new(),
                chunks: Vec::new(),
                content_length: None,
                chunked: false,
                redirected_to: None,
                send_error: None,
                transfer_error: None,
            }
        }

        /// A 200 with this body.
        pub(crate) fn ok(body: impl Into<Vec<u8>>) -> Self {
            Self::status(200).body(body)
        }

        /// The request fails before any response arrives.
        pub(crate) fn send_error(message: &str) -> Self {
            Self {
                send_error: Some(message.to_string()),
                ..Self::status(200)
            }
        }

        /// Add a response header. Names may use any case, as servers send
        /// them, and values may hold any bytes a server can send.
        pub(crate) fn header(mut self, name: &str, value: impl AsRef<[u8]>) -> Self {
            self.headers.push((
                HeaderName::from_bytes(name.as_bytes()).expect("a valid header name"),
                HeaderValue::from_bytes(value.as_ref()).expect("a valid header value"),
            ));
            self
        }

        pub(crate) fn body(self, body: impl Into<Vec<u8>>) -> Self {
            self.chunks(vec![body.into()])
        }

        /// The body arrives in these pieces.
        pub(crate) fn chunks(mut self, chunks: Vec<Vec<u8>>) -> Self {
            // Real HTTP never delivers a body with these statuses, so a test
            // that configures one would rely on something no server sends.
            let bodyless = self.status.is_informational()
                || self.status == StatusCode::NO_CONTENT
                || self.status == StatusCode::NOT_MODIFIED;
            assert!(
                !bodyless || chunks.iter().all(Vec::is_empty),
                "Nulled Http: a {} response carries no body",
                self.status
            );
            self.chunks = chunks;
            self
        }

        /// Announce no length, as a chunked or compressed transfer does.
        pub(crate) fn chunked(mut self) -> Self {
            self.chunked = true;
            self
        }

        /// Announce this length whatever the body holds. A real server
        /// cannot do this, but it lets a test reach the check on the
        /// announced length without building a body that big. An explicit
        /// length wins over [`Answer::chunked`].
        pub(crate) fn content_length(mut self, bytes: u64) -> Self {
            self.content_length = Some(bytes);
            self
        }

        /// The request was redirected, and `url` answered with this answer.
        /// Any answer configured for `url` itself plays no part.
        pub(crate) fn redirected_to(mut self, url: Url) -> Self {
            self.redirected_to = Some(url);
            self
        }

        /// The body stops with this error after its chunks.
        pub(crate) fn transfer_error(mut self, message: &str) -> Self {
            self.transfer_error = Some(message.to_string());
            self
        }

        fn unconfigured(url: &Url) -> Self {
            Self::status(503).body(format!("Nulled Http: no answer configured for {url}"))
        }
    }

    #[derive(Debug, Clone)]
    pub(super) struct StubbedClient {
        answers: Arc<Mutex<HashMap<Url, ConfigurableResponses<Answer>>>>,
    }

    impl StubbedClient {
        pub(super) fn new<A: Into<ConfigurableResponses<Answer>>>(
            answers: impl IntoIterator<Item = (Url, A)>,
        ) -> Self {
            let mut by_url = HashMap::new();
            for (url, answer) in answers {
                // A second entry would silently replace the first.
                assert!(
                    by_url.insert(url.clone(), answer.into()).is_none(),
                    "Nulled Http: {url} is configured twice; pass a Vec for a sequence"
                );
            }
            Self {
                answers: Arc::new(Mutex::new(by_url)),
            }
        }

        pub(super) fn get(&self, url: &Url) -> Result<StubbedResponse, TransportError> {
            let answer = self
                .answers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_mut(url)
                .map_or_else(
                    || Answer::unconfigured(url),
                    |answers| answers.next(&format!("Nulled Http: {url}")),
                );
            if let Some(message) = answer.send_error {
                return Err(TransportError(message));
            }
            let headers: HeaderMap = answer.headers.into_iter().collect();
            let length = answer.chunks.iter().map(Vec::len).sum::<usize>();
            let content_length = answer.content_length.or(if answer.chunked {
                None
            } else {
                Some(u64::try_from(length).expect("a test body fits in u64"))
            });
            Ok(StubbedResponse {
                status: answer.status,
                url: answer.redirected_to.unwrap_or_else(|| url.clone()),
                headers,
                content_length,
                chunks: answer.chunks.into(),
                transfer_error: answer.transfer_error,
            })
        }
    }

    #[derive(Debug)]
    pub(super) struct StubbedResponse {
        pub(super) status: StatusCode,
        pub(super) url: Url,
        pub(super) headers: HeaderMap,
        pub(super) content_length: Option<u64>,
        chunks: VecDeque<Vec<u8>>,
        transfer_error: Option<String>,
    }

    impl StubbedResponse {
        pub(super) fn chunk(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
            if let Some(chunk) = self.chunks.pop_front() {
                return Ok(Some(chunk));
            }
            match self.transfer_error.take() {
                Some(message) => Err(TransportError(message)),
                None => Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use hegel::generators;
    use url::Url;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::{Answer, Http, ReadError, Request, StatusCode, USER_AGENT};

    // Narrow integration tests: the real client against a local server.
    // They record how reqwest behaves, which is what the stub must match.

    #[tokio::test]
    async fn sends_the_user_agent_and_the_given_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let url = Url::parse(&server.uri()).unwrap();

        Http::create()
            .unwrap()
            .get(&url, &[("if-none-match", "\"abc\"")])
            .await
            .expect("request sent");

        let received = server.received_requests().await.unwrap();
        assert_eq!(received[0].headers.get("user-agent").unwrap(), USER_AGENT);
        assert_eq!(received[0].headers.get("if-none-match").unwrap(), "\"abc\"");
    }

    #[tokio::test]
    async fn reports_status_headers_length_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .set_body_string("hello"),
            )
            .mount(&server)
            .await;
        let url = Url::parse(&server.uri()).unwrap();

        let resp = Http::create().unwrap().get(&url, &[]).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.url(), &url);
        // Header lookup ignores case, as HTTP requires.
        assert_eq!(resp.header("ETag"), Some("\"v1\""));
        assert_eq!(resp.content_length(), Some(5));
        assert_eq!(resp.read_capped(5).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn reports_the_url_that_answered_after_a_redirect() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/old"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "/new"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/new"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let old = Url::parse(&format!("{}/old", server.uri())).unwrap();

        let resp = Http::create().unwrap().get(&old, &[]).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.url().path(), "/new");
    }

    #[tokio::test]
    async fn a_real_body_past_the_limit_is_too_large() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("0123456789!"))
            .mount(&server)
            .await;
        let url = Url::parse(&server.uri()).unwrap();

        let resp = Http::create().unwrap().get(&url, &[]).await.unwrap();
        assert!(matches!(
            resp.read_capped(10).await,
            Err(ReadError::TooLarge { bytes: 11 })
        ));
    }

    #[tokio::test]
    async fn a_closed_port_is_a_transport_error() {
        // Bind, note the port, and close it again, so nothing listens there.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/feed.xml")).unwrap();

        let err = Http::create().unwrap().get(&url, &[]).await.unwrap_err();
        let text = err.to_string();
        assert!(text.starts_with("error sending request"), "{text}");
        // The cause from deeper in reqwest's chain reaches the message.
        assert!(text.to_lowercase().contains("connect"), "{text}");
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[tokio::test]
    async fn a_bare_null_answers_every_url_with_a_loud_503() {
        let http = Http::create_null::<Answer>([]);
        let resp = http.get(&url("https://any.example/"), &[]).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = resp.read_capped(1024).await.unwrap();
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "Nulled Http: no answer configured for https://any.example/"
        );
    }

    #[tokio::test]
    async fn a_nulled_client_never_touches_the_network() {
        // Nothing listens on port 9 here, so only the stub can answer.
        let target = url("http://127.0.0.1:9/feed.xml");
        let http = Http::create_null([(target.clone(), Answer::ok("canned"))]);
        let resp = http.get(&target, &[]).await.unwrap();
        assert_eq!(resp.read_capped(1024).await.unwrap(), b"canned");
    }

    #[tokio::test]
    #[should_panic(expected = "No more responses configured for Nulled Http: https://a.example/")]
    async fn a_list_of_answers_is_used_up_in_order() {
        let target = url("https://a.example/");
        let http = Http::create_null([(
            target.clone(),
            vec![Answer::status(304), Answer::status(429)],
        )]);
        assert_eq!(http.get(&target, &[]).await.unwrap().status(), 304);
        assert_eq!(http.get(&target, &[]).await.unwrap().status(), 429);
        let _ = http.get(&target, &[]).await;
    }

    #[tokio::test]
    async fn tracks_each_request_from_any_clone() {
        let http = Http::create_null::<Answer>([]);
        let requests = http.track_requests();
        http.get(&url("https://a.example/"), &[("if-none-match", "\"x\"")])
            .await
            .unwrap();
        http.clone()
            .get(&url("https://b.example/"), &[])
            .await
            .unwrap();
        assert_eq!(
            requests.data(),
            [
                Request {
                    url: url("https://a.example/"),
                    headers: vec![("if-none-match", "\"x\"".to_string())],
                },
                Request {
                    url: url("https://b.example/"),
                    headers: vec![],
                },
            ]
        );
    }

    // However a body is split into chunks, reading it with a cap returns
    // the whole body when it fits. When it does not fit, reading stops at
    // the first chunk that takes the total past the cap and reports that
    // total.
    #[hegel::test]
    fn read_capped_stops_at_the_first_chunk_past_the_limit(tc: hegel::TestCase) {
        let chunks = tc.draw(
            generators::vecs(generators::vecs(generators::integers::<u8>()).max_size(16))
                .max_size(8),
        );
        // Any limit is valid. Small ones land near the body sizes drawn
        // above, where the boundary sits, so draw those often.
        let limit = tc.draw(hegel::one_of!(
            generators::integers::<usize>().max_value(150),
            generators::integers::<usize>()
        ));
        let target = url("https://a.example/");
        let http = Http::create_null([(
            target.clone(),
            Answer::status(200).chunks(chunks.clone()).chunked(),
        )]);

        let result = block_on(async {
            http.get(&target, &[])
                .await
                .unwrap()
                .read_capped(limit)
                .await
        });

        let whole = chunks.concat();
        let first_total_past_limit = chunks
            .iter()
            .scan(0, |total, chunk| {
                *total += chunk.len();
                Some(*total)
            })
            .find(|&total| total > limit);
        match (result, first_total_past_limit) {
            (Ok(body), None) => assert_eq!(body, whole),
            (Err(ReadError::TooLarge { bytes }), Some(total)) => {
                assert_eq!(bytes, u64::try_from(total).unwrap());
            }
            (other, expected) => panic!("got {other:?}, expected stop at {expected:?}"),
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    // A body that stops arriving is a transfer failure, unless the bytes
    // that did arrive already went past the limit. Reading stops there, so
    // it never reaches the failure.
    #[hegel::test]
    fn read_capped_reports_a_transfer_failure_only_if_the_body_fit(tc: hegel::TestCase) {
        let chunks = tc.draw(
            generators::vecs(generators::vecs(generators::integers::<u8>()).max_size(16))
                .max_size(8),
        );
        let limit = tc.draw(hegel::one_of!(
            generators::integers::<usize>().max_value(150),
            generators::integers::<usize>()
        ));
        let message = tc.draw(generators::text());
        let target = url("https://a.example/");
        let fits = chunks.iter().map(Vec::len).sum::<usize>() <= limit;
        let http = Http::create_null([(
            target.clone(),
            Answer::status(200).chunks(chunks).transfer_error(&message),
        )]);

        let result = block_on(async {
            http.get(&target, &[])
                .await
                .unwrap()
                .read_capped(limit)
                .await
        });

        if fits {
            assert!(
                matches!(&result, Err(ReadError::Transfer(e)) if e.to_string() == message),
                "{result:?}"
            );
        } else {
            assert!(
                matches!(result, Err(ReadError::TooLarge { .. })),
                "{result:?}"
            );
        }
    }

    // Header names match in any case, as HTTP requires, and a value made of
    // visible ASCII, spaces, and tabs reads back exactly.
    #[hegel::test]
    fn header_lookup_ignores_name_case(tc: hegel::TestCase) {
        let name = tc.draw(generators::from_regex(r"[A-Za-z][A-Za-z0-9-]{0,15}").fullmatch(true));
        let value = tc.draw(generators::from_regex(r"[\t\x20-\x7E]{0,32}").fullmatch(true));
        let configured = tc.draw(recased(&name));
        let asked = tc.draw(recased(&name));
        let target = url("https://a.example/");
        let http = Http::create_null([(
            target.clone(),
            Answer::status(200).header(&configured, &value),
        )]);

        let resp = block_on(http.get(&target, &[])).unwrap();
        assert_eq!(resp.header(&asked), Some(value.as_str()));
    }

    // A server may send bytes past ASCII in a header value. Such a value is
    // not printable text, so it reads as absent rather than as mangled text.
    #[hegel::test]
    fn header_values_past_ascii_read_as_absent(tc: hegel::TestCase) {
        let before = tc.draw(generators::from_regex(r"[\x20-\x7E]{0,8}").fullmatch(true));
        let high = tc.draw(generators::integers::<u8>().min_value(0x80));
        let after = tc.draw(generators::from_regex(r"[\x20-\x7E]{0,8}").fullmatch(true));
        let mut value = before.into_bytes();
        value.push(high);
        value.extend_from_slice(after.as_bytes());
        let target = url("https://a.example/");
        let http =
            Http::create_null([(target.clone(), Answer::status(200).header("x-test", value))]);

        let resp = block_on(http.get(&target, &[])).unwrap();
        assert_eq!(resp.header("x-test"), None);
    }

    // The same name with each letter's case chosen at random.
    #[hegel::composite]
    fn recased(tc: &hegel::TestCase, name: &str) -> String {
        name.chars()
            .map(|c| {
                if tc.draw(generators::booleans()) {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect()
    }
}
